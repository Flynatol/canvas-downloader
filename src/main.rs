#![deny(clippy::unwrap_used)]

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::ops::Add;
use std::time::Duration;
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use anyhow::{anyhow, Context, Error, Result};
use chrono::{DateTime, Local, Utc, TimeZone};
use clap::Parser;
use futures::future::{ready, join_all};
use futures::{stream, StreamExt, TryStreamExt};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use m3u8_rs::Playlist;
use rand::Rng;
use regex::Regex;
use reqwest::{header, Response, Url};
use select::document::Document;
use select::predicate::Name;
use serde_json::{json, Value};

use canvas::{File, ProcessOptions};

#[derive(Parser)]
#[command(name = "Canvas Downloader")]
#[command(version)]
struct CommandLineOptions {
    #[arg(short = 'c', long, value_name = "FILE")]
    credential_file: PathBuf,
    #[arg(short = 'd', long, value_name = "FOLDER", default_value = ".")]
    destination_folder: PathBuf,
    #[arg(short = 'n', long)]
    download_newer: bool,
    #[arg(short = 't', long, value_name = "ID", num_args(1..))]
    term_ids: Option<Vec<u32>>,
}

macro_rules! fork {
    // Motivation: recursive async functions are unsupported. We avoid this by using a non-async
    // function `f` to tokio::spawn our recursive function. Conveniently, we can wrap our barrier logic in this function
    ($f:expr, $arg:expr, $T:ty, $options:expr) => {{
        fn g(arg: $T, options: Arc<ProcessOptions>) {
            options.n_active_requests.fetch_add(1, Ordering::AcqRel);
            tokio::spawn(async move {
                let _sem = options.sem_requests.acquire().await.unwrap_or_else(|e| {
                    panic!("Please report on GitHub. Unexpected closed sem, err={e}")
                });
                let res = $f(arg, options.clone()).await;
                let new_val = options.n_active_requests.fetch_sub(1, Ordering::AcqRel) - 1;
                if new_val == 0 {
                    options.notify_main.notify_one();
                }
                if let Err(e) = res {
                    eprintln!("{e:?}");
                }
            });
        }
        g($arg, $options);
    }};
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = CommandLineOptions::parse();

    // Load credentials
    let file = std::fs::File::open(&args.credential_file)
        .with_context(|| "Could not open credential file")?;
    let cred: canvas::Credentials =
        serde_json::from_reader(file).with_context(|| "Credential file is not valid json")?;

    // Create sub-folder if not exists
    if !args.destination_folder.exists() {
        std::fs::create_dir(&args.destination_folder)
            .unwrap_or_else(|e| panic!("Failed to create destination directory, err={e}"));
    }

    // Prepare GET request options
    let client = reqwest::ClientBuilder::new()
        .tcp_keepalive(Some(Duration::from_secs(10)))
        .http2_keep_alive_interval(Some(Duration::from_secs(2)))
        .build()
        .with_context(|| "Failed to create HTTP client")?;
    let user_link = format!("{}/api/v1/users/self", cred.canvas_url);
    let user = client
        .get(&user_link)
        .bearer_auth(&cred.canvas_token)
        .send()
        .await?
        .json::<canvas::User>()
        .await
        .with_context(|| "Failed to get user info")?;
    let courses_link = format!("{}/api/v1/users/self/favorites/courses", cred.canvas_url);
    let options = Arc::new(ProcessOptions {
        canvas_token: cred.canvas_token.clone(),
        canvas_url: cred.canvas_url.clone(),
        client: client.clone(),
        user: user.clone(),
        // Process
        files_to_download: tokio::sync::Mutex::new(Vec::new()),
        download_newer: args.download_newer,
        // Download
        progress_bars: MultiProgress::new(),
        progress_style: {
            let style_template = if termsize::get().map_or(false, |size| size.cols < 100) {
                "[{wide_bar:.cyan/blue}] {total_bytes} - {msg}"
            } else {
                "[{bar:20.cyan/blue}] {bytes}/{total_bytes} - {bytes_per_sec} - {msg}"
            };
            ProgressStyle::default_bar()
                .template(style_template)
                .unwrap_or_else(|e| panic!("Please report this issue on GitHub: error with progress bar style={style_template}, err={e}"))
                .progress_chars("=>-")
        },
        // Synchronization
        n_active_requests: AtomicUsize::new(0),
        sem_requests: tokio::sync::Semaphore::new(8), // WARN magic constant.
        notify_main: tokio::sync::Notify::new(),
        // TODO handle canvas rate limiting errors, maybe scale up if possible
    });

    // Get courses
    let courses: Vec<canvas::Course> = get_pages(courses_link.clone(), &options)
        .await?
        .into_iter()
        .map(|resp| resp.json::<Vec<serde_json::Value>>()) // resp --> Result<Vec<json>>
        .collect::<stream::FuturesUnordered<_>>() // (in any order)
        .flat_map_unordered(None, |json_res| {
            let jsons = json_res.unwrap_or_else(|e| panic!("Failed to parse courses, err={e}")); // Result<Vec<json>> --> Vec<json>
            stream::iter(jsons.into_iter()) // Vec<json> --> json
        })
        .filter(|json| ready(json.get("enrollments").is_some())) // (enrolled?)
        .map(serde_json::from_value) // json --> Result<course>
        .try_collect()
        .await
        .with_context(|| "Error when getting course json")?; // Result<course> --> course

    // Filter courses by term IDs
    let Some(term_ids) = args.term_ids else {
        println!("Please provide the Term ID(s) to download via -t");
        print_all_courses_by_term(&courses);
        return Ok(());
    };
    let courses_matching_term_ids: Vec<&canvas::Course> = courses
        .iter()
        .filter(|course_json| term_ids.contains(&course_json.enrollment_term_id))
        .collect();
    if courses_matching_term_ids.is_empty() {
        println!("Could not find any course matching Term ID(s) {term_ids:?}");
        println!("Please try the following ID(s) instead");
        print_all_courses_by_term(&courses);
        return Ok(());
    }

    println!("Courses found:");
    for course in courses_matching_term_ids {
        println!("  * {} - {}", course.course_code, course.name);

        // Prep path and mkdir -p
        let course_folder_path = args
            .destination_folder
            .join(course.course_code.replace('/', "_"));
        create_folder_if_not_exist(&course_folder_path)?;
        // Prep URL for course's root folder
        let course_folders_link = format!(
            "{}/api/v1/courses/{}/folders/by_path/",
            cred.canvas_url, course.id
        );
        
        /*
        let folder_path = course_folder_path.join("files");
        fork!(
            process_folders,
            (course_folders_link, folder_path),
            (String, PathBuf),
            options.clone()
        );
         */
        
        let course_api_link = format!(
            "{}/api/v1/courses/{}/",
            cred.canvas_url, course.id
        );
        fork!(
            process_data,
            (course_api_link, course_folder_path.clone()),
            (String, PathBuf),
            options.clone()
        );

        let video_folder_path = course_folder_path.join("videos");
        create_folder_if_not_exist(&video_folder_path)?;
        fork!(
            process_videos,
            (cred.canvas_url.clone(), course.id, video_folder_path),
            (String, u32, PathBuf),
            options.clone()
        );
    }

    // Invariants
    // 1. Barrier semantics:
    //    1. Initial: n_active_requests > 0 by +1 synchronously in fork!()
    //    2. Recursion: fork()'s func +1 for subtasks before -1 own task
    //    3. --> n_active_requests == 0 only after all tasks done
    //    4. --> main() progresses only after all files have been queried
    // 2. No starvation: forks are done acyclically, all tasks +1 and -1 exactly once
    // 3. Bounded concurrency: acquire or block on semaphore before request
    // 4. No busy wait: Last task will see that there are 0 active requests and notify main
    options.notify_main.notified().await;
    assert_eq!(options.n_active_requests.load(Ordering::Acquire), 0);
    println!();

    let files_to_download = options.files_to_download.lock().await;
    println!(
        "Downloading {} file{}",
        files_to_download.len(),
        if files_to_download.len() == 1 {
            ""
        } else {
            "s"
        }
    );

    // Download files
    options.n_active_requests.fetch_add(1, Ordering::AcqRel); // prevent notifying until all spawned
    for canvas_file in files_to_download.iter() {
        fork!(
            atomic_download_file,
            canvas_file.clone(),
            File,
            options.clone()
        );
    }

    // Wait for downloads
    let new_val = options.n_active_requests.fetch_sub(1, Ordering::AcqRel) - 1;
    if new_val == 0 {
        // notify if all finished immediately
        options.notify_main.notify_one();
    }
    options.notify_main.notified().await;
    // Sanity check: running tasks trying to acquire sem will panic
    options.sem_requests.close();
    assert_eq!(options.n_active_requests.load(Ordering::Acquire), 0);

    for canvas_file in files_to_download.iter() {
        println!(
            "Downloaded {} to {}",
            canvas_file.display_name,
            canvas_file.filepath.to_string_lossy()
        );
    }

    Ok(())
}

async fn atomic_download_file(file: File, options: Arc<ProcessOptions>) -> Result<()> {
    // Create tmp file from hash
    let mut tmp_path = file.filepath.clone();
    tmp_path.pop();
    let mut h = DefaultHasher::new();
    file.display_name.hash(&mut h);
    tmp_path.push(&h.finish().to_string().add(".tmp"));

    // Aborted download?
    if let Err(e) = download_file((&tmp_path, &file), options.clone()).await {
        if let Err(e) = std::fs::remove_file(&tmp_path) {
            eprintln!(
                "Failed to remove temporary file {tmp_path:?} for {}, err={e:?}",
                file.display_name
            );
        }
        return Err(e);
    }

    // Update file time
    let updated_at = DateTime::parse_from_rfc3339(&file.updated_at)?;
    let updated_time = filetime::FileTime::from_unix_time(
        updated_at.timestamp(),
        updated_at.timestamp_subsec_nanos(),
    );
    if let Err(e) = filetime::set_file_mtime(&tmp_path, updated_time) {
        eprintln!(
            "Failed to set modified time of {} with updated_at of {}, err={e:?}",
            file.display_name, file.updated_at
        )
    }

    // Atomically rename file, doesn't change mtime
    std::fs::rename(&tmp_path, &file.filepath)?;
    Ok(())
}

async fn download_file(
    (tmp_path, canvas_file): (&PathBuf, &File),
    options: Arc<ProcessOptions>,
) -> Result<()> {
    // Get file
    let mut resp = options
        .client
        .get(&canvas_file.url)
        .bearer_auth(&options.canvas_token)
        .send()
        .await
        .with_context(|| format!("Something went wrong when reaching {}", canvas_file.url))?;
    if !resp.status().is_success() {
        return Err(Error::msg(format!(
            "Failed to download {}, got {resp:?}",
            canvas_file.display_name
        )));
    }

    // Create + Open file
    let mut file = std::fs::File::create(tmp_path)
        .with_context(|| format!("Unable to create tmp file for {:?}", canvas_file.filepath))?;

    // Progress bar
    let download_size = resp
        .headers() // Gives us the HeaderMap
        .get(header::CONTENT_LENGTH) // Gives us an Option containing the HeaderValue
        .and_then(|ct_len| ct_len.to_str().ok()) // Unwraps the Option as &str
        .and_then(|ct_len| ct_len.parse().ok()) // Parses the Option as u64
        .unwrap_or(0); // Fallback to 0
    let progress_bar = options.progress_bars.add(ProgressBar::new(download_size));
    progress_bar.set_message(canvas_file.display_name.to_string());
    progress_bar.set_style(options.progress_style.clone());

    // Download
    while let Some(chunk) = resp.chunk().await? {
        progress_bar.inc(chunk.len() as u64);
        let mut cursor = std::io::Cursor::new(chunk);
        std::io::copy(&mut cursor, &mut file)
            .with_context(|| format!("Could not write to file {:?}", canvas_file.filepath))?;
    }

    progress_bar.finish();
    Ok(())
}

fn print_all_courses_by_term(courses: &[canvas::Course]) {
    let mut grouped_courses: HashMap<u32, Vec<&str>> = HashMap::new();

    for course in courses.iter() {
        let course_id: u32 = course.enrollment_term_id;
        grouped_courses
            .entry(course_id)
            .or_insert_with(Vec::new)
            .push(&course.course_code);
    }
    println!("{: <10}| {:?}", "Term IDs", "Courses");
    for (key, value) in &grouped_courses {
        println!("{: <10}| {:?}", key, value);
    }
}

fn create_folder_if_not_exist(folder_path: &PathBuf) -> Result<()> {
    if !folder_path.exists() {
        std::fs::create_dir(&folder_path).with_context(|| {
            format!(
                "Failed to create directory: {}",
                folder_path.to_string_lossy()
            )
        })?;
    }
    Ok(())
}

// async recursion needs boxing
async fn process_folders(
    (url, path): (String, PathBuf),
    options: Arc<ProcessOptions>,
) -> Result<()> {
    let pages = get_pages(url, &options).await?;

    // For each page
    for pg in pages {
        let uri = pg.url().to_string();
        let folders_result = pg.json::<canvas::FolderResult>().await;

        match folders_result {
            // Got folders
            Ok(canvas::FolderResult::Ok(folders)) => {
                for folder in folders {
                    // println!("  * {} - {}", folder.id, folder.name);
                    let sanitized_folder_name = sanitize_foldername(folder.name);
                    // if the folder has no parent, it is the root folder of a course
                    // so we avoid the extra directory nesting by not appending the root folder name
                    let folder_path = if folder.parent_folder_id.is_some() {
                        path.join(sanitized_folder_name)
                    } else {
                        path.clone()
                    };
                    if !folder_path.exists() {
                        if let Err(e) = std::fs::create_dir(&folder_path) {
                            eprintln!(
                                "Failed to create directory: {}, err={e}",
                                folder_path.to_string_lossy()
                            );
                            continue;
                        };
                    }

                    fork!(
                        process_files,
                        (folder.files_url, folder_path.clone()),
                        (String, PathBuf),
                        options.clone()
                    );
                    fork!(
                        process_folders,
                        (folder.folders_url, folder_path),
                        (String, PathBuf),
                        options.clone()
                    );
                }
            }

            // Got status code
            Ok(canvas::FolderResult::Err { status }) => {
                let course_has_no_folders = status == "unauthorized";
                if !course_has_no_folders {
                    eprintln!(
                        "Failed to access folders at link:{uri}, path:{path:?}, status:{status}",
                    );
                }
            }

            // Parse error
            Err(e) => {
                eprintln!("Error when getting folders at link:{uri}, path:{path:?}\n{e:?}",);
            }
        }
    }

    Ok(())
}

async fn process_videos(
    (url, id, path):
    (String, u32, PathBuf),
    options: Arc<ProcessOptions>,
) -> Result<()> {
    let session = get_canvas_api(format!("{}/login/session_token?return_to={}/courses/{}/external_tools/128", url, url, id), &options).await?;
    let session_result = session.json::<canvas::Session>().await?;

    // Need a new client for each session for the cookie store
    let client = reqwest::ClientBuilder::new()
        .cookie_store(true)
        .build()?;
    let videos = client
        .get(session_result.session_url)
        .send()
        .await?;

    // Parse the form that contains the parameters needed to request
    let video_html = videos.text().await?;
    let (action, params) = {
        let panopto_document = Document::from_read(video_html.as_bytes())?;
        let panopto_form = panopto_document
            .find(Name("form"))
            .filter(|n| n.attr("data-tool-id") == Some("mediaweb.ap.panopto.com"))
            .next()
            .ok_or(anyhow!("Could not find panopto form"))?;
        let action = panopto_form
            .attr("action")
            .ok_or(anyhow!("Could not find panopto form action"))?
            .to_string();
        let params = panopto_form
            .find(Name("input"))
            .filter_map(|n| n.attr("name").map(|name| (name.to_string(), n.attr("value").unwrap_or("").to_string())))
            .collect::<Vec<(_, _)>>();
        (action, params)
    };
    // set origin and referral headers
    let panopto_response = client
        .post(action)
        .header("Origin", &url)
        .header("Referer", format!("{}/", url))
        .form(&params)
        .send()
        .await?;

    // parse location header as url
    let panopto_location = Url::parse(panopto_response
        .headers()
        .get(header::LOCATION)
        .ok_or(anyhow!("No location header"))?
        .to_str()?)?;
    // get folderID from query string
    let panopto_folder_id = panopto_location
        .query_pairs()
        .find(|(key, _)| key == "folderID")
        .map(|(_, value)| value)
        .ok_or(anyhow!("Could not get Panopto Folder ID"))?
        .to_string();
    let panopto_host = panopto_location
        .host_str()
        .ok_or(anyhow!("Could not get Panopto Host"))?
        .to_string();
    process_video_folder((panopto_host, panopto_folder_id, client.clone(), path), options).await?;
    Ok(())
}

async fn process_video_folder(
    (host, id, client, path):
    (String, String, reqwest::Client, PathBuf),
    options: Arc<ProcessOptions>,
) -> Result<()> {
    // POST json folderID: to https://mediaweb.ap.panopto.com/Panopto/Services/Data.svc/GetFolderInfo
    let folderinfo_result = client
        .post(format!("https://{}/Panopto/Services/Data.svc/GetFolderInfo", host))
        .json(&json!({
            "folderID": id,
        }))
        .send()
        .await?;
    // write into videos.json
    let folderinfo = folderinfo_result.text().await?;
    let mut file = std::fs::File::create(path.join("folder.json"))?;
    file.write_all(folderinfo.as_bytes())?;

    // write into sessions.json
    let mut sessions_file = std::fs::File::create(path.join("sessions.json"))?;

    for i in 0.. {
        let sessions_result = client
            .post(format!("https://{}/Panopto/Services/Data.svc/GetSessions", host))
            .json(&json!({
                "queryParameters":
                {
                    "query":null,
                    "sortColumn":1,
                    "sortAscending":false,
                    "maxResults":100,
                    "page":i,
                    "startDate":null,
                    "endDate":null,
                    "folderID":id,
                    "bookmarked":false,
                    "getFolderData":true,
                    "isSharedWithMe":false,
                    "isSubscriptionsPage":false,
                    "includeArchived":true,
                    "includeArchivedStateCount":true,
                    "sessionListOnlyArchived":false,
                    "includePlaylists":true
                }
            }))
            .send()
            .await?;

        let sessions_text = sessions_result.text().await?;
        sessions_file.write_all(sessions_text.as_bytes())?;
        
        let folder_sessions = serde_json::from_str::<Value>(&sessions_text)?;
        let folder_sessions_results = folder_sessions
            .get("d")
            .ok_or(anyhow!("Could not get Panopto Folder Sessions"))?;
    
        let sessions = serde_json::from_value::<canvas::PanoptoSessionInfo>(folder_sessions_results.clone())?;
        
        // End of page results
        if sessions.Results.len() == 0 {
            break;
        }
        for result in sessions.Results {
            fork!(
                process_session,
                (host.clone(), result, client.clone(), path.clone()),
                (String, canvas::PanoptoResult, reqwest::Client, PathBuf),
                options.clone()
            )
        }
        // Subfolders are the same, so process only the first request
        if i == 0 {
            for subfolder in sessions.Subfolders {
                let subfolder_path = path.join(sanitize_foldername(subfolder.Name));
                create_folder_if_not_exist(&subfolder_path)?;
                fork!(
                    process_video_folder,
                    (host.clone(), subfolder.ID, client.clone(), subfolder_path),
                    (String, String, reqwest::Client, PathBuf),
                    options.clone()
                );
            }
        }
    }
    Ok(())
}

async fn process_session(
    (host, result, client, path):
    (String, canvas::PanoptoResult, reqwest::Client, PathBuf),
    options: Arc<ProcessOptions>,
) -> Result<()> {
    // POST deliveryID: to https://mediaweb.ap.panopto.com/Panopto/Pages/Viewer/DeliveryInfo.aspx
    let resp = client
        .post(format!("https://{}/Panopto/Pages/Viewer/DeliveryInfo.aspx", host))
        .form(&[
            ("deliveryId",result.DeliveryID.as_str()),
            ("invocationId",""),
            ("isLiveNotes","false"),
            ("refreshAuthCookie","true"),
            ("isActiveBroadcast","false"),
            ("isEditing","false"),
            ("isKollectiveAgentInstalled","false"),
            ("isEmbed","false"),
            ("responseType","json"),
        ])
        .send()
        .await?;

    let delivery_info = resp.json::<canvas::PanoptoDeliveryInfo>().await?;
    
    let viewer_file_id = delivery_info.ViewerFileId;
    let panopto_url = Url::parse(&result.IosVideoUrl)?;
    let panopto_cdn_host = panopto_url.host_str().unwrap_or("s-cloudfront.cdn.ap.panopto.com");
    let panopto_master_m3u8 = format!("https://{}/sessions/{}/{}-{}.hls/master.m3u8", panopto_cdn_host, result.SessionID, result.DeliveryID, viewer_file_id);
    let m3u8_resp = client
        .get(panopto_master_m3u8)
        .send()
        .await?;
    let m3u8_text = m3u8_resp.text().await?;
    let m3u8_parser = m3u8_rs::parse_playlist_res(m3u8_text.as_bytes());
    match m3u8_parser {
        Ok(Playlist::MasterPlaylist(pl)) => {
            // get the highest bandwidth
            let download_variant = pl.variants
                .iter()
                .max_by_key(|v| v.bandwidth)
                .unwrap();

            let panopto_index_m3u8 = format!("https://{}/sessions/{}/{}-{}.hls/{}", panopto_cdn_host, result.SessionID, result.DeliveryID, viewer_file_id, download_variant.uri);
            
            let index_m3u8_resp = client
                .get(panopto_index_m3u8)
                .send()
                .await?;
            let index_m3u8_text = index_m3u8_resp.text().await?;
            let index_m3u8_parser = m3u8_rs::parse_playlist_res(index_m3u8_text.as_bytes());
            match index_m3u8_parser {
                Ok(Playlist::MasterPlaylist(_index_pl)) => {},
                Ok(Playlist::MediaPlaylist(index_pl)) => {
                    let uri_id = download_variant.uri.split("/").next().ok_or(anyhow!("Could not get URI ID"))?;
                    let file_uri = index_pl.segments[0].uri.clone();
                    let file_uri_ext = Path::new(&file_uri).extension().unwrap_or(OsStr::new("")).to_str().unwrap_or("");
                    let panopto_mp4_file = format!("https://{}/sessions/{}/{}-{}.hls/{}/{}", panopto_cdn_host, result.SessionID, result.DeliveryID, viewer_file_id, uri_id, file_uri);
                    let download_file_name = if file_uri_ext == "" {
                        format!("{}", result.SessionName)
                    } else {
                        format!("{}.{}", result.SessionName, file_uri_ext)
                    };

                    let date_regex = Regex::new(r"/Date\((\d+)\)/").unwrap();
                    let date_match_rfc3339 = date_regex
                        .captures(&result.StartTime)
                        .and_then(|x| x.get(1))
                        .map(|x| x.as_str())
                        .ok_or(anyhow!("Parse error for StartTime"))
                        .and_then(|x| x.parse::<i64>().map_err(|e| anyhow!("Conversion error for StartTime: {}", e)))
                        .and_then(|x| Utc.timestamp_millis_opt(x).earliest().ok_or(anyhow!("Timestamp parse error for StartTime")))
                        .map(|x| x.to_rfc3339())?;

                    let file = canvas::File {
                        display_name: download_file_name,
                        folder_id: 0,
                        id: 0,
                        size: 0,
                        url: panopto_mp4_file,
                        locked_for_user: false,
                        updated_at: date_match_rfc3339,
                        filepath: path.clone(),
                    };
                    let mut lock = options.files_to_download.lock().await;
                    let mut filtered_files = filter_files(&options, &path, [file].to_vec());
                    lock.append(&mut filtered_files);
                },
                Err(e) => println!("Error: {:?}", e),
            }
            
        }
        Ok(Playlist::MediaPlaylist(_pl)) => {},
        Err(e) => println!("Error: {:?}", e),
    }

    Ok(())
}

async fn process_data(
    (url, path): (String, PathBuf),
    options: Arc<ProcessOptions>,
) -> Result<()> {
    let assignments_path = path.join("assignments");
    create_folder_if_not_exist(&assignments_path)?;
    fork!(
        process_assignments,
        (url.clone(), assignments_path),
        (String, PathBuf),
        options.clone()
    );
    let users_path = path.join("users.json");
    fork!(
        process_users,
        (url.clone(), users_path),
        (String, PathBuf),
        options.clone()
    );
    let discussions_path = path.join("discussions");
    create_folder_if_not_exist(&discussions_path)?;
    fork!(
        process_discussions,
        (url.clone(), false, discussions_path),
        (String, bool, PathBuf),
        options.clone()
    );
    let announcements_path = path.join("announcements");
    create_folder_if_not_exist(&announcements_path)?;
    fork!(
        process_discussions,
        (url.clone(), true, announcements_path),
        (String, bool, PathBuf),
        options.clone()
    );

    
    /*
    I do not need this

    let pages_path = path.join("pages");
    create_folder_if_not_exist(&pages_path)?;
    fork!(
        process_pages,
        (url.clone(), pages_path),
        (String, PathBuf),
        options.clone()
    );
     */

    let modules_path = path.join("modules");
    create_folder_if_not_exist(&modules_path)?;
    fork!(
        process_modules,
        (url.clone(), modules_path),
        (String, PathBuf),
        options.clone()
    );

    Ok(())
}

async fn process_pages(
    (url, path): (String, PathBuf),
    options: Arc<ProcessOptions>,
) -> Result<()> {
    let pages_url = format!("{}pages", url);
    let pages = get_pages(pages_url, &options).await?;
    
    let pages_path = path.join("pages.json");
    let mut pages_file = std::fs::File::create(pages_path.clone())
        .with_context(|| format!("Unable to create file for {:?}", pages_path))?;

    for pg in pages {
        let uri = pg.url().to_string();
        let page_body = pg.text().await?;

        pages_file
            .write_all(page_body.as_bytes())
            .with_context(|| format!("Could not write to file {:?}", pages_path))?;

        let page_result = serde_json::from_str::<canvas::PageResult>(&page_body);

        match page_result {
            Ok(canvas::PageResult::Ok(pages)) => {
                for page in pages {
                    let page_url = format!("{}pages/{}", url, page.url);
                    let page_file_path = path.join(sanitize_foldername(page.url.clone()));
                    create_folder_if_not_exist(&page_file_path)?;
                    fork!(
                        process_page_body,
                        (page_url, page.url, page_file_path),
                        (String, String, PathBuf),
                        options.clone()
                    )
                }
            }

            Ok(canvas::PageResult::Err { status }) => {
                eprintln!("No pages found for url {} status: {}", uri, status);
            }

            Err(e) => {
                eprintln!("No pages found for url {} error: {}", uri, e);
            }
        };
    }

    Ok(())
}

async fn process_page_body(
    (url, title, path): (String, String, PathBuf),
    options: Arc<ProcessOptions>,
) -> Result<()> {
    let page_resp = get_canvas_api(url.clone(), &options).await?;

    let page_file_path = path.join(format!("{}.json", sanitize_filename::sanitize(title)));
    let mut page_file = std::fs::File::create(page_file_path.clone())
        .with_context(|| format!("Unable to create file for {:?}", page_file_path))?;

    let page_resp_text = page_resp.text().await?;
    page_file
        .write_all(page_resp_text.as_bytes())
        .with_context(|| format!("Could not write to file {:?}", page_file_path))?;

    let page_body_result = serde_json::from_str::<canvas::PageBody>(&page_resp_text);
    match page_body_result {
        Result::Ok(page_body) => {
            let page_html = format!(
                "<html><head><title>{}</title></head><body>{}</body></html>",
                page_body.title, page_body.body);
            
            let page_html_path = path.join(format!("{}.html", sanitize_filename::sanitize(page_body.url)));
            let mut page_html_file = std::fs::File::create(page_html_path.clone())
                .with_context(|| format!("Unable to create file for {:?}", page_html_path))?;

            page_html_file
                .write_all(page_html.as_bytes())
                .with_context(|| format!("Could not write to file {:?}", page_html_path))?;
            
            fork!(
                process_html_links,
                (page_html, path),
                (String, PathBuf),
                options.clone()
            )
        }
        Result::Err(e) => {
            eprintln!("Error when parsing page body at link:{url}, path:{page_file_path:?}\n{e:?}",);
        }
    }
    Ok(())
}

async fn process_assignments(
    (url, path): (String, PathBuf),
    options: Arc<ProcessOptions>,
) -> Result<()> {
    let assignments_url = format!("{}assignments?include[]=submission&include[]=assignment_visibility&include[]=all_dates&include[]=overrides&include[]=observed_users&include[]=can_edit&include[]=score_statistics", url);
    let pages = get_pages(assignments_url, &options).await?;
    
    let assignments_json = path.join("assignments.json");
    let mut assignments_file = std::fs::File::create(assignments_json.clone())
        .with_context(|| format!("Unable to create file for {:?}", assignments_json))?;

    for pg in pages {
        let uri = pg.url().to_string();
        let page_body = pg.text().await?;

        assignments_file
            .write_all(page_body.as_bytes())
            .with_context(|| format!("Unable to write to file for {:?}", assignments_json))?;

        let assignment_result = serde_json::from_str::<canvas::AssignmentResult>(&page_body);

        match assignment_result {
            Ok(canvas::AssignmentResult::Ok(assignments)) => {
                for assignment in assignments {
                    let assignment_path = path.join(sanitize_foldername(assignment.name));
                    create_folder_if_not_exist(&assignment_path)?;
                    let submissions_url = format!("{}assignments/{}/submissions/", url, assignment.id);
                    fork!(
                        process_submissions,
                        (submissions_url, assignment_path.clone()),
                        (String, PathBuf),
                        options.clone()
                    );
                    fork!(
                        process_html_links,
                        (assignment.description, assignment_path),
                        (String, PathBuf),
                        options.clone()
                    );
                }
            }
            Ok(canvas::AssignmentResult::Err { status }) => {
                eprintln!(
                    "Failed to access assignments at link:{uri}, path:{path:?}, status:{status}",
                );
            }
            Err(e) => {
                eprintln!("Error when getting assignments at link:{uri}, path:{path:?}\n{e:?}",);
            }
        }
    }
    Ok(())
}

async fn process_submissions(
    (url, path): (String, PathBuf),
    options: Arc<ProcessOptions>,
) -> Result<()> {
    let submissions_url = format!("{}{}", url, options.user.id);

    let resp = get_canvas_api(submissions_url, &options).await?;
    let submissions_body = resp.text().await?;
    let submissions_json = path.join("submission.json");
    let mut submissions_file = std::fs::File::create(submissions_json.clone())
        .with_context(|| format!("Unable to create file for {:?}", submissions_json))?;

    submissions_file
        .write_all(submissions_body.as_bytes())
        .with_context(|| format!("Unable to write to file for {:?}", submissions_json))?;

    let submissions_result = serde_json::from_str::<canvas::Submission>(&submissions_body);
    match submissions_result {
        Result::Ok(submissions) => {
            let mut filtered_files = filter_files(&options, &path, submissions.attachments);
            let mut lock = options.files_to_download.lock().await;
            lock.append(&mut filtered_files);
        }
        Result::Err(e) => {
            eprintln!("Error when getting submissions at link:{url}, path:{path:?}\n{e:?}",);
        }
    }
    Ok(())
}

async fn process_users (
    (url, path): (String, PathBuf),
    options: Arc<ProcessOptions>,
) -> Result<()> {
    let users_url = format!("{}users?include_inactive=true&include[]=avatar_url&include[]=enrollments&include[]=email&include[]=observed_users&include[]=can_be_removed&include[]=custom_links", url);
    let pages = get_pages(users_url, &options).await?;
    
    let users_path = sanitize_filename::sanitize(path.to_string_lossy());
    let mut users_file = std::fs::File::create(path.clone())
        .with_context(|| format!("Unable to create file for {:?}", users_path))?;

    for pg in pages {
        let page_body = pg.text().await?;
        
        users_file
            .write_all(page_body.as_bytes())
            .with_context(|| format!("Unable to write to file for {:?}", users_path))?;
    }

    Ok(())
}

async fn process_discussions(
    (url, announcement, path): (String, bool, PathBuf),
    options: Arc<ProcessOptions>,
) -> Result<()> {
    let discussion_url = format!("{}discussion_topics{}", url, if announcement { "?only_announcements=true" } else { "" });
    let pages = get_pages(discussion_url, &options).await?;

    let discussion_path = path.join("discussions.json");
    let mut discussion_file = std::fs::File::create(discussion_path.clone())
        .with_context(|| format!("Unable to create file for disc {:?}", discussion_path))?;

    for pg in pages {
        let uri = pg.url().to_string();
        let page_body = pg.text().await?;

        discussion_file
            .write_all(page_body.as_bytes())
            .with_context(|| format!("Unable to write to file for {:?}", discussion_path))?;

        let discussion_result = serde_json::from_str::<canvas::DiscussionResult>(&page_body);

        match discussion_result {
            Ok(canvas::DiscussionResult::Ok(discussions)) => {
                for discussion in discussions {
                    // download attachments
                    let discussion_folder_path = path.join(format!("{}_{}", discussion.id, sanitize_foldername(discussion.title)));
                    create_folder_if_not_exist(&discussion_folder_path)?;

                    let files = discussion.attachments
                        .into_iter()
                        .map(|mut f| {
                            f.display_name = format!("{}_{}", f.id, &f.display_name);
                            f
                        })
                        .collect();
                    {
                        let mut filtered_files = filter_files(&options, &discussion_folder_path, files);
                        let mut lock = options.files_to_download.lock().await;
                        lock.append(&mut filtered_files);
                    }
                    
                    fork!(
                        process_html_links,
                        (discussion.message, discussion_folder_path.clone()),
                        (String, PathBuf),
                        options.clone()
                    );
                    let view_url = format!("{}discussion_topics/{}/view", url, discussion.id);
                    fork!(
                        process_discussion_view,
                        (view_url, discussion_folder_path),
                        (String, PathBuf),
                        options.clone()
                    )
                }
            }
            Ok(canvas::DiscussionResult::Err { status }) => {
                eprintln!(
                    "Failed to access discussions at link:{uri}, path:{path:?}, status:{status}",
                );
            }
            Err(e) => {
                eprintln!("Error when getting discussions at link:{uri}, path:{path:?}\n{e:?}",);
            }
        }
    }
    Ok(())
}


async fn process_modules(
    (url, path): (String, PathBuf),
    options: Arc<ProcessOptions>,
) -> Result<()> {
    let module_url = format!("{}modules", url);
    let pages = get_pages(module_url, &options).await?;

    let module_path = path.join("modules.json");
    let mut module_file = std::fs::File::create(module_path.clone())
        .with_context(|| format!("Unable to create file for {:?}", module_path))?;

    for pg in pages {
        let uri = pg.url().to_string();
        let page_body = pg.text().await?;

        module_file
            .write_all(page_body.as_bytes())
            .with_context(|| format!("Unable to write to file for {:?}", module_path))?;
        
        
        let module_result = serde_json::from_str::<canvas::ModuleResult>(&page_body);

        match module_result {
            Ok(canvas::ModuleResult::Ok(module_sections)) => {
                for module_section in module_sections {
                    // download attachments
                    let module_section_folder_path = path.join(format!("{}_{}", module_section.id, sanitize_foldername(module_section.name)));
                    create_folder_if_not_exist(&module_section_folder_path)?;

                    fork!(
                        process_module_items,
                        (module_section.items_url, module_section_folder_path.clone()),
                        (String, PathBuf),
                        options.clone()
                    );
                }
            }
            Ok(canvas::ModuleResult::Err { status }) => {
                eprintln!(
                    "Failed to access modules at link:{uri}, path:{path:?}, status:{status}",
                );
            }
            Err(e) => {
                eprintln!("Error when getting modules at link:{uri}, path:{path:?}\n{e:?}",);
            }
        }
    }
    Ok(())
}


async fn process_module_items(
    (url, path): (String, PathBuf),
    options: Arc<ProcessOptions>,
) -> Result<()> {
    let page = get_canvas_api(url, &options).await?;

    let item_path = path.join("items.json");
    let mut item_file = std::fs::File::create(item_path.clone())
        .with_context(|| format!("Unable to create file for {:?}", item_path))?;

    let uri = page.url().to_string();
    let page_body = page.text().await?;

    item_file
        .write_all(page_body.as_bytes())
        .with_context(|| format!("Unable to write to file for {:?}", item_path))?;
   
    
    let item_result = serde_json::from_str::<canvas::ModuleItemsResult>(&page_body);

    match item_result {
        Ok(canvas::ModuleItemsResult::Ok(module_items)) => {
            for item in module_items {
                let item_folder_path = path.join(format!("{}_{}", item.id, sanitize_foldername(item.title.clone())));
                create_folder_if_not_exist(&item_folder_path)?;

                //This is not a great solution, but it works for now
                if item.Type == "Page" {
                    fork!(
                        process_page_body,
                        (item.url.unwrap(), item.title, item_folder_path),
                        (String, String, PathBuf),
                        options.clone()
                    );
                } else if item.Type == "File" {
                    let pg = get_canvas_api(item.url.clone().unwrap(), &options).await?;
                    let files_result = pg.json::<canvas::File>().await;


                    match files_result {
                        // Got files
                        Ok(file) => {
                            let mut filtered_files = filter_files(&options, &item_folder_path, vec![file]);
                            let mut lock = options.files_to_download.lock().await;
                            lock.append(&mut filtered_files);
                        }
                     
                        // Parse error
                        Err(e) => {
                            eprintln!("Error when getting files at link:{uri}, path:{path:?}\n{e:?}",);
                        }
                    };
        

                }
            }
        }
        Ok(canvas::ModuleItemsResult::Err { status }) => {
            eprintln!(
                "Failed to access module items at link:{uri}, path:{path:?}, status:{status}",
            );
        }
        Err(e) => {
            eprintln!("Error when getting module items at link:{uri}, path:{path:?}\n{e:?}",);
            eprintln!("content was {page_body}",);
        }
    }
    
    Ok(())
}


async fn process_discussion_view(
    (url, path): (String, PathBuf),
    options: Arc<ProcessOptions>,
) -> Result<()> {
    let resp = get_canvas_api(url.clone(), &options).await?;
    let discussion_view_body = resp.text().await?;
    
    let discussion_view_json = path.join("discussion.json");
    let mut discussion_view_file = std::fs::File::create(discussion_view_json.clone())
        .with_context(|| format!("Unable to create file for v {:?}", discussion_view_json))?;

    discussion_view_file
        .write_all(discussion_view_body.as_bytes())
        .with_context(|| format!("Unable to write to file for {:?}", discussion_view_json))?;

    let discussion_view_result = serde_json::from_str::<canvas::DiscussionView>(&discussion_view_body);
    let mut attachments_all = Vec::new();
    match discussion_view_result {
        Result::Ok(discussion_view) => {
            for view in discussion_view.view {
                if let Some(message) = view.message {
                    fork!(
                        process_html_links,
                        (message, path.clone()),
                        (String, PathBuf),
                        options.clone()
                    )
                }
                if let Some(mut attachments) = view.attachments {
                    attachments_all.append(&mut attachments);
                }
                if let Some(attachment) = view.attachment {
                    attachments_all.push(attachment);
                }
            }
        }
        Result::Err(e) => {
            eprintln!("Error when getting submissions at link:{url}, path:{path:?}\n{e:?}",);
        }
    }

    let files = attachments_all
        .into_iter()
        .map(|mut f| {
            f.display_name = format!("{}_{}", f.id, &f.display_name);
            f
        })
        .collect();
    let mut filtered_files = filter_files(&options, &path, files);
    let mut lock = options.files_to_download.lock().await;
    lock.append(&mut filtered_files);

    Ok(())
}

async fn process_files((url, path): (String, PathBuf), options: Arc<ProcessOptions>) -> Result<()> {
    let pages = get_pages(url, &options).await?;

    // For each page
    for pg in pages {
        let uri = pg.url().to_string();

        let files_result = pg.json::<canvas::FileResult>().await;

        match files_result {
            // Got files
            Ok(canvas::FileResult::Ok(files)) => {
                let mut filtered_files = filter_files(&options, &path, files);
                let mut lock = options.files_to_download.lock().await;
                lock.append(&mut filtered_files);
            }

            // Got status code
            Ok(canvas::FileResult::Err { status }) => {
                let course_has_no_files = status == "unauthorized";
                if !course_has_no_files {
                    eprintln!(
                        "Failed to access files at link:{uri}, path:{path:?}, status:{status}",
                    );
                }
            }

            // Parse error
            Err(e) => {
                eprintln!("Error when getting files at link:{uri}, path:{path:?}\n{e:?}",);
            }
        };
    }

    Ok(())
}

fn filter_files(options: &ProcessOptions, path: &Path, files: Vec<File>) -> Vec<File> {
    fn updated(filepath: &PathBuf, new_modified: &str) -> bool {
        (|| -> Result<bool> {
            let old_modified = std::fs::metadata(filepath)?.modified()?;
            let new_modified =
                std::time::SystemTime::from(DateTime::parse_from_rfc3339(new_modified)?);
            let updated = old_modified < new_modified;
            if updated {
                println!("Found update for {filepath:?}. Use -n to download updated files.");
            }
            Ok(updated)
        })()
        .unwrap_or(false)
    }

    // only download files that do not exist or are updated
    files
        .into_iter()
        .map(|mut f| {
            let sanitized_filename = sanitize_filename::sanitize(&f.display_name);
            f.filepath = path.join(sanitized_filename);
            f
        })
        .filter(|f| !f.locked_for_user)
        .filter(|f| {
            if DateTime::parse_from_rfc3339(&f.updated_at).is_ok() {
                return true;
            }
            eprintln!(
                "Failed to parse updated_at time for {}, {}",
                f.display_name, f.updated_at
            );
            false
        })
        .filter(|f| {
            !f.filepath.exists() || (updated(&f.filepath, &f.updated_at) && options.download_newer)
        })
        .collect()
}

async fn process_html_links(
    (html, path): (String, PathBuf),
    options: Arc<ProcessOptions>,
) -> Result<()> {

    // If file link is part of course files
    let re = Regex::new(r"/courses/[0-9]+/files/[0-9]+").unwrap();
    let file_links = Document::from(html.as_str())
        .find(Name("a"))
        .filter_map(|n| n.attr("href"))
        .filter(|x| x.starts_with(&options.canvas_url))
        .map(|x| Url::parse(x))
        .filter(|x| x.is_ok())
        .map(|x| x.unwrap())
        .filter(|x| re.is_match(x.path()))
        .map(|x| format!("{}/api/v1{}", options.canvas_url, x.path()))
        .collect::<Vec<String>>();
    
    let mut link_files = join_all(file_links.into_iter()
        .map(|x| process_file_id((x, path.clone()), options.clone())))
        .await
        .into_iter()
        .filter_map(|x| x.ok())
        .collect::<Vec<File>>();

    // If image is from canvas it is likely the file url gives permission denied, so download from the CDN
    let image_links = Document::from(html.as_str())
        .find(Name("img"))
        .filter_map(|n| n.attr("src"))
        .filter(|x| x.starts_with(&options.canvas_url))
        .filter(|x| !x.contains("equation_images"))
        .map(|x| x.to_string())
        .collect::<Vec<String>>();
    
    link_files.append(join_all(image_links.into_iter()
        .map(|x| prepare_link_for_download((x, path.clone()), options.clone())))
        .await
        .into_iter()
        .filter_map(|x| x.ok())
        .collect::<Vec<File>>().as_mut());

    let mut filtered_files = filter_files(&options, &path, link_files);
    let mut lock = options.files_to_download.lock().await;
    lock.append(&mut filtered_files);

    Ok(())
}

async fn process_file_id(
    (url, path): (String, PathBuf),
    options: Arc<ProcessOptions>,
) -> Result<File> {
    let url = url.trim_end_matches("/download");

    let file_resp = get_canvas_api(url.to_string(), &options).await?;
    let file_result = file_resp.json::<canvas::File>().await;
    match file_result {
        Result::Ok(mut file) => {
            let file_path = path.join(&file.display_name);
            file.filepath = file_path;
            return Ok(file);
        }
        Err(e) => {
            eprintln!("Error when getting file info at link:{url}, path:{path:?}\n{e:?}",);
            return Err(Into::into(e));
        }
    }
}
async fn prepare_link_for_download(
    (link, path): (String, PathBuf),
    options: Arc<ProcessOptions>,
) -> Result<File> {

    let resp = options
        .client
        .head(&link)
        .bearer_auth(&options.canvas_token)
        .timeout(Duration::from_secs(10))
        .send()
        .await?;
    let headers = resp.headers();
    // get filename out of Content-Disposition header
    let filename = headers
        .get(header::CONTENT_DISPOSITION)
        .and_then(|x| x.to_str().ok())
        .and_then(|x| {
            let re = Regex::new(r#"filename="(.*)""#).unwrap();
            re.captures(x)
        })
        .and_then(|x| x.get(1))
        .map(|x| x.as_str())
        .unwrap_or_else(|| {
            let re = Regex::new(r"/([^/]+)$").unwrap();
            re.captures(&link)
                .and_then(|x| x.get(1))
                .map(|x| x.as_str())
                .unwrap_or("unknown")
        });
    // last-modified header to TZ string
    let updated_at = headers
        .get(header::LAST_MODIFIED)
        .and_then(|x| x.to_str().ok())
        .and_then(|x| {
            let dt = DateTime::parse_from_rfc2822(x).ok()?;
            Some(dt.with_timezone(&Local).to_rfc3339())
        })
        .unwrap_or_else(|| Local::now().to_rfc3339());
    
    let file = File {
        id: 0,
        folder_id: 0,
        display_name: filename.to_string(),
        size: 0,
        url: link.clone(),
        updated_at: updated_at,
        locked_for_user: false,
        filepath: path.join(filename),
    };
    Ok(file)
}

async fn get_pages(link: String, options: &ProcessOptions) -> Result<Vec<Response>> {
    fn parse_next_page(resp: &Response) -> Option<String> {
        // Parse LINK header
        let links = resp.headers().get(header::LINK)?.to_str().ok()?; // ok to not have LINK header
        let rels = parse_link_header::parse_with_rel(links).unwrap_or_else(|e| {
            panic!(
                "Error parsing header for next page, uri={}, err={e:?}",
                resp.url()
            )
        });

        // Is last page?
        let nex = rels.get("next")?; // ok to not have "next"
        let cur = rels
            .get("current")
            .unwrap_or_else(|| panic!("Could not find current page for {}", resp.url()));
        let last = rels
            .get("last")?;
        if cur == last {
            return None;
        };

        // Next page
        Some(nex.raw_uri.clone())
    }

    let mut link = Some(link);
    let mut resps = Vec::new();

    while let Some(uri) = link {
        // GET request
        let resp = get_canvas_api(uri, options).await?;

        // Get next page before returning for json
        link = parse_next_page(&resp);
        resps.push(resp);
    }
    Ok(resps)
}

fn sanitize_foldername<S: AsRef<str>>(name: S) -> String {
    let name = name.as_ref();
    let rex = Regex::new(r#"[/\?<.">\\:\*\|":]"#).unwrap();

    let name_modified = rex.replace_all(&name, "");

    return String::from(name_modified.trim());
}

async fn get_canvas_api(url: String, options: &ProcessOptions) -> Result<Response> {
    let mut query_pairs : Vec<(String, String)> = Vec::new();
    // insert into query_pairs from url.query_pairs();
    for (key, value) in Url::parse(&url)?.query_pairs() {
        query_pairs.push((key.to_string(), value.to_string()));
    }
    for retry in 0..3 {
        let resp = options
            .client
            .get(&url)
            .query(&query_pairs)
            .bearer_auth(&options.canvas_token)
            .timeout(Duration::from_secs(10))
            .send()
            .await;

        match resp {
            Ok(resp) => {
                if resp.status() != reqwest::StatusCode::FORBIDDEN || retry == 2 {
                    return Ok(resp)
                }
            },
            Err(e) => {println!("Canvas request error uri: {} {}", url, e); return Err(e.into())},
        }

        let wait_time = Duration::from_millis(rand::thread_rng().gen_range(0..1000 * 2_u64.pow(retry)));
        println!("Got 403 for {}, waiting {:?} before retrying, retry {}", url, wait_time, retry);
        tokio::time::sleep(wait_time).await;
        
    }
    Err(Error::msg("canvas request failed"))
}

mod canvas {
    use std::sync::atomic::AtomicUsize;

    use serde::{Deserialize, Serialize};
    use tokio::sync::Mutex;

    #[derive(Clone, Deserialize, Serialize)]
    #[serde(rename_all = "camelCase")]
    pub struct Credentials {
        pub canvas_url: String,
        pub canvas_token: String,
    }

    #[derive(Deserialize)]
    pub struct Course {
        pub id: u32,
        pub name: String,
        pub course_code: String,
        pub enrollment_term_id: u32,
    }

    #[derive(Clone, Debug, Deserialize)]
    pub struct User {
        pub id: u32,
        pub name: String,
    }

    #[derive(Deserialize)]
    #[serde(untagged)]
    pub(crate) enum FolderResult {
        Err { status: String },
        Ok(Vec<Folder>),
    }

    #[derive(Deserialize)]
    pub struct Folder {
        pub id: u32,
        pub name: String,
        pub folders_url: String,
        pub files_url: String,
        pub for_submissions: bool,
        pub can_upload: bool,
        pub parent_folder_id: Option<u32>,
    }

    #[derive(Deserialize)]
    #[serde(untagged)]
    pub(crate) enum FileResult {
        Err { status: String },
        Ok(Vec<File>),
    }

    #[derive(Deserialize)]
    #[serde(untagged)]
    pub(crate) enum PageResult {
        Err { status: String },
        Ok(Vec<Page>),
    }

    #[derive(Clone, Debug, Deserialize)]
    pub struct Page {
        pub page_id: u32,
        pub url: String,
        pub title: String,
        pub updated_at: String,
        pub locked_for_user: bool,
    }

    #[derive(Clone, Debug, Deserialize)]
    pub struct PageBody {
        pub page_id: u32,
        pub url: String,
        pub title: String,
        pub body: String,
        pub updated_at: String,
        pub locked_for_user: bool,
    }

    #[derive(Clone, Debug, Deserialize)]
    pub struct ModuleSection {
        pub id: u32,
        pub items_url: String,
        pub name: String,
    }

    #[derive(Clone, Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]

    pub struct ModuleItem {
        pub id: u32,
        pub title: String,
        pub Type: String,
        #[serde(default)]
        pub url: Option<String>,
    }


    #[derive(Deserialize)]
    #[serde(untagged)]
    pub(crate) enum ModuleResult {
        Err { status: String },
        Ok(Vec<ModuleSection>),
    }

    #[derive(Deserialize)]
    #[serde(untagged)]
    pub(crate) enum ModuleItemsResult {
        Err { status: String },
        Ok(Vec<ModuleItem>),
    }

    #[derive(Deserialize)]
    #[serde(untagged)]
    pub(crate) enum AssignmentResult {
        Err { status: String },
        Ok(Vec<Assignment>),
    }
    #[derive(Clone, Debug, Deserialize)]
    pub struct Assignment {
        pub id: u32,
        pub name: String,
        pub description: String,
    }

    #[derive(Clone, Debug, Deserialize)]
    pub struct Submission {
        pub id: u32,
        pub body: Option<String>,
        pub attachments: Vec<File>,
    }
    
    #[derive(Deserialize)]
    #[serde(untagged)]
    pub(crate) enum DiscussionResult {
        Err { status: String },
        Ok(Vec<Discussion>),
    }
    #[derive(Clone, Debug, Deserialize)]
    pub struct Discussion {
        pub id: u32,
        pub title: String,
        pub message: String,
        pub attachments: Vec<File>,
    }

    #[derive(Clone, Debug, Deserialize)]
    pub struct DiscussionView {
        pub unread_entries: Vec<u32>,
        pub view: Vec<Comments>,
    }

    #[derive(Clone, Debug, Deserialize)]
    pub struct Comments {
        pub id: u32,
        pub message: Option<String>,
        pub attachment: Option<File>,
        pub attachments: Option<Vec<File>>,
    }

    #[derive(Clone, Debug, Deserialize)]
    pub struct File {
        pub id: u32,
        pub folder_id: u32,
        pub display_name: String,
        pub size: u64,
        pub url: String,
        pub updated_at: String,
        pub locked_for_user: bool,
        #[serde(skip)]
        pub filepath: std::path::PathBuf,
    }

    #[derive(Clone, Debug, Deserialize)]
    pub struct Session {
        pub session_url: String,
        pub requires_terms_acceptance: bool,
    }

    #[derive(Clone, Debug, Deserialize)]
    #[allow(non_snake_case)]
    pub struct PanoptoSessionInfo {
        pub TotalNumber: u32,
        pub Results: Vec<PanoptoResult>,
        pub Subfolders: Vec<PanoptoSubfolder>,
    }

    #[derive(Clone, Debug, Deserialize)]
    #[allow(non_snake_case)]
    pub struct PanoptoResult {
        pub DeliveryID: String,
        pub FolderID: String,
        pub SessionID: String,
        pub SessionName: String,
        pub StartTime: String,
        pub IosVideoUrl: String,
    }

    #[derive(Clone, Debug, Deserialize)]
    #[allow(non_snake_case)]
    pub struct PanoptoSubfolder {
        pub ID: String,
        pub Name: String,
    }
    
    #[derive(Clone, Debug, Deserialize)]
    #[allow(non_snake_case)]
    pub struct PanoptoDeliveryInfo {
        pub SessionId: String,
        pub ViewerFileId: String,
    }

    pub struct ProcessOptions {
        pub canvas_token: String,
        pub canvas_url: String,
        pub client: reqwest::Client,
        pub user: User,
        // Process
        pub download_newer: bool,
        pub files_to_download: Mutex<Vec<File>>,
        // Download
        pub progress_bars: indicatif::MultiProgress,
        pub progress_style: indicatif::ProgressStyle,
        // Synchronization
        pub n_active_requests: AtomicUsize, // main() waits for this to be 0
        pub sem_requests: tokio::sync::Semaphore, // Limit #active requests
        pub notify_main: tokio::sync::Notify,
    }
}
