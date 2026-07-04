//! GitHub integration: issue/PR search for the wizard and background polling of
//! linked issues/PRs so a task's notification dot lights up on new activity.
//!
//! Runs on a dedicated thread with its own Tokio runtime. The UI thread submits
//! [`Request`]s over a std channel and drains [`GhEvent`]s over an async channel
//! (the same pattern the toast/tray subsystems use). The feature is enabled only
//! when a `GITHUB_TOKEN` is present; otherwise [`spawn`] returns `None` and the
//! rest of the app runs unchanged.

/// A search result (an issue or pull request).
#[derive(Clone, Debug)]
pub struct IssueHit {
    pub slug: String,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub updated_at: i64,
}

/// Work submitted to the GitHub thread from the UI.
pub enum Request {
    /// Search issues/PRs matching a free-text query (for the wizard).
    Search { query: String },
    /// Check a linked issue/PR for its latest `updated_at`.
    Poll { task_id: u64, slug: String, number: u64 },
}

/// Results delivered back to the UI thread.
pub enum GhEvent {
    SearchResults(Vec<IssueHit>),
    /// The linked item's current `updated_at` (epoch seconds) and web URL.
    Activity { task_id: u64, updated_at: i64, url: String, title: String },
    Error(String),
}

/// Handle to the running GitHub subsystem.
pub struct Github {
    pub requests: std::sync::mpsc::Sender<Request>,
    pub events: async_channel::Receiver<GhEvent>,
}

/// The token the subsystem would use: the one configured in the settings when
/// non-empty, else the `GITHUB_TOKEN` environment variable.
pub fn resolve_token(settings_token: &str) -> Option<String> {
    let configured = settings_token.trim();
    if !configured.is_empty() {
        return Some(configured.to_string());
    }
    std::env::var("GITHUB_TOKEN").ok().filter(|t| !t.trim().is_empty())
}

/// Start the GitHub subsystem with `token`. Returns `None` (feature off) when
/// the thread can't be started; the subsystem shuts down when the returned
/// handle (its request sender) is dropped. Restart with a new token by dropping
/// the old handle and calling this again.
pub fn spawn(token: String) -> Option<Github> {
    let (req_tx, req_rx) = std::sync::mpsc::channel::<Request>();
    let (ev_tx, ev_rx) = async_channel::unbounded::<GhEvent>();

    std::thread::Builder::new()
        .name("focuswm-github".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    log::warn!("github: could not start runtime: {e}");
                    return;
                }
            };
            let octo = {
                // Build inside the runtime: octocrab's HTTP client wants a
                // reactor available, and building it outside would panic.
                let _guard = rt.enter();
                match octocrab::Octocrab::builder().personal_token(token).build() {
                    Ok(o) => o,
                    Err(e) => {
                        log::warn!("github: could not build client: {e}");
                        return;
                    }
                }
            };
            rt.block_on(async move {
                // Blocking recv between requests is fine: we serve one at a time.
                while let Ok(req) = req_rx.recv() {
                    match req {
                        Request::Search { query } => {
                            let event = match search(&octo, &query).await {
                                Ok(hits) => GhEvent::SearchResults(hits),
                                Err(e) => GhEvent::Error(format!("GitHub search failed: {e}")),
                            };
                            let _ = ev_tx.send(event).await;
                        }
                        Request::Poll { task_id, slug, number } => {
                            if let Ok(Some(hit)) = poll(&octo, &slug, number).await {
                                let _ = ev_tx
                                    .send(GhEvent::Activity {
                                        task_id,
                                        updated_at: hit.updated_at,
                                        url: hit.url,
                                        title: hit.title,
                                    })
                                    .await;
                            }
                        }
                    }
                }
            });
        })
        .ok()?;

    Some(Github { requests: req_tx, events: ev_rx })
}

/// Search issues and pull requests, newest activity first, capped to a handful.
async fn search(octo: &octocrab::Octocrab, query: &str) -> octocrab::Result<Vec<IssueHit>> {
    let page = octo
        .search()
        .issues_and_pull_requests(query)
        .sort("updated")
        .order("desc")
        .per_page(10)
        .send()
        .await?;
    Ok(page.items.into_iter().filter_map(hit_from_issue).take(10).collect())
}

/// Fetch a single linked issue/PR's current state.
async fn poll(
    octo: &octocrab::Octocrab,
    slug: &str,
    number: u64,
) -> octocrab::Result<Option<IssueHit>> {
    let Some((owner, repo)) = focuswm_shell::parse_slug(slug) else {
        return Ok(None);
    };
    let issue = octo.issues(owner, repo).get(number).await?;
    Ok(hit_from_issue(issue))
}

/// Convert an octocrab issue/PR into our lightweight [`IssueHit`].
fn hit_from_issue(issue: octocrab::models::issues::Issue) -> Option<IssueHit> {
    let url = issue.html_url.to_string();
    let (owner, repo) = focuswm_shell::parse_slug(&url)?;
    Some(IssueHit {
        slug: format!("{owner}/{repo}"),
        number: issue.number,
        title: issue.title,
        url,
        updated_at: issue.updated_at.timestamp(),
    })
}
