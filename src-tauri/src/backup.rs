// Git-versioned outline backup of the whole thought store.
//
// Each "backup" exports every graph as a Markdown outline file into
// `~/Documents/my-thoughtgraph`, then commits the directory with git. The
// frontend exposes an interactive history browser; restoring a version
// re-imports its outlines as NEW graphs (non-destructive — see commands.rs /
// main.js). Outline format mirrors the import format already understood by the
// frontend parser: `# <graph name>`, then indented `-` bullets per node.

use anyhow::{anyhow, Context, Result};
use chrono::Local;
use rusqlite::Connection;
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::db::{self, Node};

/// `~/Documents/my-thoughtgraph` — the git-managed backup directory.
pub fn backup_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join("Documents/my-thoughtgraph")
}

// GUI apps launched from Finder get a minimal PATH, so probe the usual spots
// before falling back to a shell lookup (same approach as graph::which_dot).
fn which_git() -> Result<String> {
    for candidate in &["/usr/bin/git", "/usr/local/bin/git", "/opt/homebrew/bin/git"] {
        if Path::new(candidate).exists() {
            return Ok((*candidate).to_string());
        }
    }
    let out = Command::new("sh").arg("-c").arg("command -v git").output()?;
    if out.status.success() {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() {
            return Ok(s);
        }
    }
    Err(anyhow!(
        "`git` not found. Install the Xcode command line tools (`xcode-select --install`)."
    ))
}

// Run git and fail loudly on a non-zero exit.
fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let git_bin = which_git()?;
    let out = Command::new(&git_bin)
        .current_dir(dir)
        .args(args)
        .output()
        .with_context(|| format!("running git {:?}", args))?;
    if !out.status.success() {
        return Err(anyhow!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

// Run git but never error — used for probes (`config`, `log` on an empty repo,
// etc.) where a non-zero exit is expected and just means "empty".
fn git_opt(dir: &Path, args: &[&str]) -> String {
    let git_bin = match which_git() {
        Ok(b) => b,
        Err(_) => return String::new(),
    };
    match Command::new(&git_bin).current_dir(dir).args(args).output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).into_owned(),
        _ => String::new(),
    }
}

fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() {
        "untitled".to_string()
    } else {
        s
    }
}

// Render one graph as a Markdown outline. Reply-children nest under their
// parent by indentation; multi-line node content keeps its extra lines as
// indented continuation lines so the file stays readable.
fn render_outline(conn: &Connection, graph: &db::Graph) -> Result<String> {
    let nodes = db::list_nodes(conn, graph.id)?; // id ASC == creation order
    let edges = db::list_edges(conn, graph.id)?;

    let mut reply_parent: HashMap<i64, i64> = HashMap::new();
    for e in &edges {
        if e.kind == "reply" {
            reply_parent.insert(e.to_node_id, e.from_node_id);
        }
    }
    let mut children: HashMap<i64, Vec<Node>> = HashMap::new();
    let mut roots: Vec<Node> = Vec::new();
    for n in &nodes {
        match reply_parent.get(&n.id) {
            Some(pid) => children.entry(*pid).or_default().push(n.clone()),
            None => roots.push(n.clone()),
        }
    }

    let mut out = String::new();
    out.push_str(&format!("# {}\n\n", graph.name));
    if !graph.description.trim().is_empty() {
        out.push_str(&format!("{}\n\n", graph.description.trim()));
    }

    fn emit(out: &mut String, node: &Node, depth: usize, children: &HashMap<i64, Vec<Node>>) {
        let indent = "  ".repeat(depth);
        let mut lines = node.content.split('\n');
        let first = lines.next().unwrap_or("");
        out.push_str(&format!("{}- {}\n", indent, first));
        for cont in lines {
            // continuation line: align under the bullet text (indent + "  ")
            out.push_str(&format!("{}  {}\n", indent, cont));
        }
        if let Some(kids) = children.get(&node.id) {
            for k in kids {
                emit(out, k, depth + 1, children);
            }
        }
    }

    for r in &roots {
        emit(&mut out, r, 0, &children);
    }
    if roots.is_empty() {
        out.push_str("_(no nodes yet)_\n");
    }
    Ok(out)
}

fn ensure_repo(dir: &Path) -> Result<()> {
    if !dir.join(".git").exists() {
        git(dir, &["init", "-q"])?;
        // A README so the very first commit is never empty / the folder reads well.
        let readme = dir.join("README.md");
        if !readme.exists() {
            std::fs::write(
                &readme,
                "# my-thoughtgraph\n\nGit-versioned outline backups from ThoughtGraph.\n",
            )?;
        }
    }
    // Make sure commits won't fail on a machine without a global git identity.
    if git_opt(dir, &["config", "user.email"]).trim().is_empty() {
        git(dir, &["config", "user.email", "thoughtgraph@localhost"])?;
    }
    if git_opt(dir, &["config", "user.name"]).trim().is_empty() {
        git(dir, &["config", "user.name", "ThoughtGraph"])?;
    }
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct BackupResult {
    pub committed: bool,
    pub message: String,
    pub hash: Option<String>,
    pub path: String,
    pub graphs: usize,
}

/// Export every graph to the backup dir and commit. No-op commit if unchanged.
pub fn backup_now(conn: &Connection) -> Result<BackupResult> {
    let dir = backup_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    ensure_repo(&dir)?;

    // Remove the .md files we manage so deleted/renamed graphs propagate. The
    // README and the .git dir are preserved.
    for entry in std::fs::read_dir(&dir)? {
        let p = entry?.path();
        if p.is_file()
            && p.extension().map(|e| e == "md").unwrap_or(false)
            && p.file_name().map(|n| n != "README.md").unwrap_or(true)
        {
            std::fs::remove_file(&p).ok();
        }
    }

    let graphs = db::list_graphs(conn)?;
    for g in &graphs {
        let outline = render_outline(conn, g)?;
        let fname = format!("{:04}-{}.md", g.id, sanitize(&g.name));
        std::fs::write(dir.join(fname), outline)?;
    }

    git(&dir, &["add", "-A"])?;
    let status = git(&dir, &["status", "--porcelain"])?;
    let path = dir.to_string_lossy().into_owned();
    if status.trim().is_empty() {
        return Ok(BackupResult {
            committed: false,
            message: "No changes since the last backup.".to_string(),
            hash: None,
            path,
            graphs: graphs.len(),
        });
    }

    let ts = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let message = format!("Backup {} — {} graph(s)", ts, graphs.len());
    git(&dir, &["commit", "-q", "-m", &message])?;
    let hash = git(&dir, &["rev-parse", "HEAD"])?.trim().to_string();
    Ok(BackupResult {
        committed: true,
        message,
        hash: Some(hash),
        path,
        graphs: graphs.len(),
    })
}

#[derive(Debug, Serialize)]
pub struct Commit {
    pub hash: String,
    pub short: String,
    pub date: String,
    pub message: String,
}

/// Most-recent-first commit list. Empty if the repo has no commits yet.
pub fn history(limit: usize) -> Result<Vec<Commit>> {
    let dir = backup_dir();
    if !dir.join(".git").exists() {
        return Ok(vec![]);
    }
    // \x1f field separator, one commit per line.
    let fmt = "--pretty=format:%H\x1f%h\x1f%cI\x1f%s";
    let n = format!("-n{}", limit);
    let out = git_opt(&dir, &["log", fmt, &n]);
    let mut commits = Vec::new();
    for line in out.lines() {
        let parts: Vec<&str> = line.splitn(4, '\u{1f}').collect();
        if parts.len() == 4 {
            commits.push(Commit {
                hash: parts[0].to_string(),
                short: parts[1].to_string(),
                date: parts[2].to_string(),
                message: parts[3].to_string(),
            });
        }
    }
    Ok(commits)
}

#[derive(Debug, Serialize)]
pub struct CommitFile {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Serialize)]
pub struct CommitDetail {
    pub hash: String,
    pub stat: String,
    pub files: Vec<CommitFile>,
}

/// The full outline snapshot at a commit: every `.md` file's content as it was,
/// plus a `--stat` summary. Used for the read-only viewer and for restore.
pub fn commit_detail(hash: &str) -> Result<CommitDetail> {
    let dir = backup_dir();
    if !dir.join(".git").exists() {
        return Err(anyhow!("No backup repository yet."));
    }
    let stat = git(&dir, &["show", "--stat", "--oneline", "--no-color", hash])?;
    let listing = git(&dir, &["ls-tree", "-r", "--name-only", hash])?;
    let mut files = Vec::new();
    for path in listing.lines() {
        if !path.ends_with(".md") || path == "README.md" {
            continue;
        }
        let content = git(&dir, &["show", &format!("{}:{}", hash, path)])?;
        files.push(CommitFile {
            path: path.to_string(),
            content,
        });
    }
    Ok(CommitDetail {
        hash: hash.to_string(),
        stat,
        files,
    })
}
