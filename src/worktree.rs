use crate::{
    model::{WORKTREE_SCHEMA_VERSION, WorktreeProvenance},
    storage,
};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io,
    path::{Component, Path, PathBuf},
    process::{Command, Output},
};
use thiserror::Error;

const MAX_WORKTREE_RECORDS: usize = 128;
const MAX_METADATA_BYTES: u64 = 1 << 20;
const MAX_ID_BYTES: usize = 512;
const CLAIMS_FILE: &str = "relay-worktree-claims.json";
const CATALOG_FILE: &str = "relay-worktree-history.json";
const LOCK_FILE: &str = "relay-worktree-coordination.lock";

#[derive(Debug, Error)]
pub enum WorktreeError {
    #[error("could not run Git in {directory}: {source}")]
    Spawn {
        directory: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("Git command failed in {directory}: {message}")]
    Command { directory: PathBuf, message: String },
    #[error("Git returned non-UTF-8 output in {directory}")]
    NonUtf8 { directory: PathBuf },
    #[error("malformed Git worktree metadata: {0}")]
    Malformed(String),
    #[error("unsafe Git worktree metadata: {0}")]
    Unsafe(String),
    #[error("worktree claim conflict: {0}")]
    Conflict(String),
    #[error(transparent)]
    Storage(#[from] storage::StorageError),
}

#[derive(Debug, Clone, Serialize)]
pub struct WorktreeListing {
    pub available: bool,
    pub repository_id: Option<String>,
    pub repository_root: Option<String>,
    pub worktrees: Vec<WorktreeView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorktreeView {
    #[serde(flatten)]
    pub provenance: WorktreeProvenance,
    pub current: bool,
    pub stale: bool,
    pub deleted: bool,
    pub prunable: bool,
    pub locked: bool,
    pub active: WorktreeActivity,
    pub sessions: Vec<String>,
    pub runs: Vec<String>,
    pub handoffs: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct WorktreeActivity {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub enum ClaimKind {
    Session,
    Run,
}

impl ClaimKind {
    fn field(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Run => "run",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CoordinationFile {
    schema_version: u32,
    repository_id: String,
    #[serde(default)]
    claims: Vec<Claim>,
    #[serde(default)]
    worktrees: Vec<WorktreeProvenance>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Claim {
    worktree_id: String,
    worktree_path: String,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    run: Option<String>,
}

#[derive(Debug, Clone)]
struct GitWorktree {
    path: PathBuf,
    head: Option<String>,
    branch: Option<String>,
    detached: bool,
    prunable: bool,
    locked: bool,
}

#[derive(Debug)]
struct WorktreeState {
    branch: Option<String>,
    ref_name: Option<String>,
    head: Option<String>,
    detached: bool,
}

type AssociatedState = (Vec<String>, Vec<String>, Vec<String>);

pub fn current(directory: &Path) -> Result<Option<WorktreeProvenance>, WorktreeError> {
    let Some(root) = repository_root(directory)? else {
        return Ok(None);
    };
    let worktree_path = canonical(&root, "worktree path")?;
    let repository_id = metadata_path(directory, &["rev-parse", "--git-common-dir"])?;
    let repository_root = repository_id
        .parent()
        .ok_or_else(|| {
            WorktreeError::Malformed("Git common directory has no repository root".into())
        })?
        .to_owned();
    let repository_root = canonical(&repository_root, "repository root")?;
    let worktree_id = metadata_path(directory, &["rev-parse", "--git-dir"])?;
    let branch = optional_text(directory, &["symbolic-ref", "--quiet", "--short", "HEAD"])?;
    let ref_name = optional_text(directory, &["symbolic-ref", "--quiet", "HEAD"])?;
    let head = optional_text(directory, &["rev-parse", "--verify", "HEAD"])?;
    Ok(Some(provenance(
        repository_id,
        repository_root,
        worktree_id,
        worktree_path,
        WorktreeState {
            detached: branch.is_none(),
            branch,
            ref_name,
            head,
        },
    )?))
}

pub fn list(directory: &Path) -> Result<WorktreeListing, WorktreeError> {
    let Some(current) = current(directory)? else {
        return Ok(WorktreeListing {
            available: false,
            repository_id: None,
            repository_root: None,
            worktrees: Vec::new(),
        });
    };
    let repository_root = PathBuf::from(&current.repository_root);
    let repository_id = PathBuf::from(&current.repository_id);
    let output = run(directory, &["worktree", "list", "--porcelain", "-z"])?;
    if !output.status.success() {
        return Err(command_error(directory, output));
    }
    let records = parse_worktrees(&output.stdout)?;
    let mut claims = Vec::new();
    let mut catalog = with_lock(&repository_id, |path| {
        read_coordination(path, &current.repository_id)
    })?;
    let mut views = Vec::with_capacity(records.len() + catalog.worktrees.len());
    let mut seen = HashSet::new();
    for record in records {
        let path = absolute_path(&repository_root, &record.path);
        let worktree_id = worktree_id_for(&repository_id, &repository_root, &path)?;
        let item = provenance(
            current.repository_id.clone().into(),
            repository_root.clone(),
            worktree_id.into(),
            path.clone(),
            WorktreeState {
                branch: record.branch.clone(),
                ref_name: record.branch.clone(),
                head: record.head.clone(),
                detached: record.detached,
            },
        )?;
        seen.insert(item.worktree_id.clone());
        catalog
            .worktrees
            .retain(|old| old.worktree_id != item.worktree_id);
        catalog.worktrees.push(item.clone());
        views.push(view_for(
            item,
            &current,
            record.prunable,
            record.locked,
            &claims,
            &path,
        )?);
    }
    catalog.worktrees.truncate(MAX_WORKTREE_RECORDS);
    let mut stored = catalog.worktrees.clone();
    stored.sort_by(|left, right| left.worktree_id.cmp(&right.worktree_id));
    for item in stored {
        if seen.contains(&item.worktree_id) {
            continue;
        }
        let path = PathBuf::from(&item.worktree_path);
        views.push(view_for(item, &current, true, false, &claims, &path)?);
    }
    views.sort_by(|left, right| {
        left.provenance
            .worktree_path
            .cmp(&right.provenance.worktree_path)
    });
    claims = load_claims(&repository_id, &current.repository_id)?;
    for view in &mut views {
        apply_claims(view, &claims);
    }
    with_lock(&repository_id, |path| {
        let mut file = read_coordination(path, &current.repository_id)?;
        file.worktrees = views.iter().map(|view| view.provenance.clone()).collect();
        file.worktrees.truncate(MAX_WORKTREE_RECORDS);
        write_coordination(path, &file)
    })?;
    Ok(WorktreeListing {
        available: true,
        repository_id: Some(current.repository_id),
        repository_root: Some(current.repository_root),
        worktrees: views,
    })
}

pub fn select(
    listing: &WorktreeListing,
    selector: &str,
    invocation: &Path,
) -> Result<WorktreeProvenance, WorktreeError> {
    let candidate = absolute_path(invocation, Path::new(selector));
    let matches = listing
        .worktrees
        .iter()
        .filter(|item| {
            same_path(Path::new(&item.provenance.worktree_path), &candidate)
                || item.provenance.worktree_id == selector
                || item.provenance.branch.as_deref() == Some(selector)
                || item.provenance.ref_name.as_deref() == Some(selector)
                || item.provenance.head.as_deref() == Some(selector)
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [item] => Ok(item.provenance.clone()),
        [] => Err(WorktreeError::Malformed(format!(
            "no Git worktree matches {selector:?}"
        ))),
        _ => Err(WorktreeError::Malformed(format!(
            "Git worktree selector {selector:?} is ambiguous"
        ))),
    }
}

pub fn claim(
    provenance: &WorktreeProvenance,
    kind: ClaimKind,
    id: &str,
) -> Result<(), WorktreeError> {
    validate_provenance(provenance)?;
    validate_id(id, "claim id")?;
    let common = PathBuf::from(&provenance.repository_id);
    with_lock(&common, |path| {
        let mut file = read_coordination(path, &provenance.repository_id)?;
        let index = if let Some(index) = file
            .claims
            .iter()
            .position(|claim| claim.worktree_id == provenance.worktree_id)
        {
            index
        } else {
            if file.claims.len() >= MAX_WORKTREE_RECORDS {
                return Err(WorktreeError::Malformed(
                    "worktree claim limit exceeded".into(),
                ));
            }
            file.claims.push(Claim {
                worktree_id: provenance.worktree_id.clone(),
                worktree_path: provenance.worktree_path.clone(),
                session: None,
                run: None,
            });
            file.claims.len() - 1
        };
        let existing = match kind {
            ClaimKind::Session => file.claims[index].session.as_deref(),
            ClaimKind::Run => file.claims[index].run.as_deref(),
        };
        if let Some(existing) = existing
            && existing != id
        {
            return Err(WorktreeError::Conflict(format!(
                "worktree {} already has an active {} claim ({existing}); finish or inspect that operation first",
                provenance.worktree_path,
                kind.field()
            )));
        }
        if file.claims.iter().enumerate().any(|(candidate, claim)| {
            candidate != index
                && match kind {
                    ClaimKind::Session => claim.session.as_deref() == Some(id),
                    ClaimKind::Run => claim.run.as_deref() == Some(id),
                }
        }) {
            return Err(WorktreeError::Conflict(format!(
                "{} {id} is already claimed by another Git worktree",
                kind.field()
            )));
        }
        let entry = &mut file.claims[index];
        match kind {
            ClaimKind::Session => entry.session = Some(id.to_owned()),
            ClaimKind::Run => entry.run = Some(id.to_owned()),
        }
        entry.worktree_path = provenance.worktree_path.clone();
        write_coordination(path, &file)
    })
}

pub fn release(
    provenance: &WorktreeProvenance,
    kind: ClaimKind,
    id: &str,
) -> Result<(), WorktreeError> {
    validate_provenance(provenance)?;
    let common = PathBuf::from(&provenance.repository_id);
    with_lock(&common, |path| {
        let mut file = read_coordination(path, &provenance.repository_id)?;
        if let Some(entry) = file
            .claims
            .iter_mut()
            .find(|claim| claim.worktree_id == provenance.worktree_id)
        {
            let slot = match kind {
                ClaimKind::Session => &mut entry.session,
                ClaimKind::Run => &mut entry.run,
            };
            if slot.as_deref() == Some(id) {
                *slot = None;
            }
        }
        file.claims
            .retain(|claim| claim.session.is_some() || claim.run.is_some());
        write_coordination(path, &file)
    })
}

fn view_for(
    provenance: WorktreeProvenance,
    current: &WorktreeProvenance,
    prunable: bool,
    locked: bool,
    claims: &[Claim],
    path: &Path,
) -> Result<WorktreeView, WorktreeError> {
    let deleted = !path.exists();
    let (sessions, runs, handoffs) = associated_state(path)?;
    let mut view = WorktreeView {
        current: provenance.worktree_id == current.worktree_id,
        stale: prunable || deleted,
        deleted,
        prunable,
        locked,
        active: WorktreeActivity::default(),
        sessions,
        runs,
        handoffs,
        provenance,
    };
    apply_claims(&mut view, claims);
    Ok(view)
}

fn apply_claims(view: &mut WorktreeView, claims: &[Claim]) {
    if let Some(claim) = claims
        .iter()
        .find(|claim| claim.worktree_id == view.provenance.worktree_id)
    {
        view.active.session = claim.session.clone();
        view.active.run = claim.run.clone();
    }
}

fn load_claims(common: &Path, repository_id: &str) -> Result<Vec<Claim>, WorktreeError> {
    with_lock(common, |path| {
        Ok(read_coordination(path, repository_id)?.claims)
    })
}

fn associated_state(path: &Path) -> Result<AssociatedState, WorktreeError> {
    let relay = path.join(".relay");
    let metadata = match fs::symlink_metadata(&relay) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok((Vec::new(), Vec::new(), Vec::new()));
        }
        Err(error) => return Err(WorktreeError::Unsafe(error.to_string())),
    };
    if metadata.file_type().is_symlink() {
        return Err(WorktreeError::Unsafe(format!(
            "symlinked Relay state path {}",
            relay.display()
        )));
    }
    if !metadata.is_dir() {
        return Err(WorktreeError::Malformed(format!(
            "Relay state path is not a directory: {}",
            relay.display()
        )));
    }
    let sessions = child_ids(&relay.join("sessions"), "session")?;
    let runs = child_ids(&relay.join("runs"), "run")?;
    let handoffs = if relay.join("handoffs.jsonl").exists() {
        let bytes = bounded_read(&relay.join("handoffs.jsonl"), MAX_METADATA_BYTES)?;
        bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| {
                serde_json::from_slice::<serde_json::Value>(line)
                    .map_err(|error| WorktreeError::Malformed(format!("handoff ledger: {error}")))
                    .and_then(|value| {
                        value
                            .get("id")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                            .ok_or_else(|| {
                                WorktreeError::Malformed("handoff ledger record has no id".into())
                            })
                    })
            })
            .take(MAX_WORKTREE_RECORDS)
            .collect::<Result<Vec<_>, _>>()?
    } else {
        Vec::new()
    };
    Ok((sessions, runs, handoffs))
}

fn child_ids(directory: &Path, kind: &str) -> Result<Vec<String>, WorktreeError> {
    let metadata = match fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(WorktreeError::Unsafe(error.to_string())),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(WorktreeError::Unsafe(format!(
            "unsafe {kind} state directory {}",
            directory.display()
        )));
    }
    let mut values = Vec::new();
    for entry in
        fs::read_dir(directory).map_err(|error| WorktreeError::Unsafe(error.to_string()))?
    {
        let entry = entry.map_err(|error| WorktreeError::Unsafe(error.to_string()))?;
        let file_type = entry
            .file_type()
            .map_err(|error| WorktreeError::Unsafe(error.to_string()))?;
        if file_type.is_symlink() {
            return Err(WorktreeError::Unsafe(format!(
                "symlinked {kind} state {}",
                entry.path().display()
            )));
        }
        if file_type.is_file()
            && entry.path().extension().and_then(|value| value.to_str()) == Some("json")
        {
            values.push(
                entry
                    .path()
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        if values.len() > MAX_WORKTREE_RECORDS {
            return Err(WorktreeError::Malformed(format!("too many {kind} records")));
        }
    }
    values.sort();
    Ok(values)
}

fn worktree_id_for(common: &Path, root: &Path, path: &Path) -> Result<String, WorktreeError> {
    if path == root {
        return path_string(common, "Git common directory");
    }
    if path.is_dir()
        && let Ok(value) = metadata_path(path, &["rev-parse", "--git-dir"])
    {
        return path_string(&value, "Git worktree directory");
    }
    let admin = common.join("worktrees");
    let metadata = match fs::symlink_metadata(&admin) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(WorktreeError::Malformed(format!(
                "no Git metadata identifies worktree {}",
                path.display()
            )));
        }
        Err(error) => return Err(WorktreeError::Unsafe(error.to_string())),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(WorktreeError::Unsafe(format!(
            "unsafe Git worktrees directory {}",
            admin.display()
        )));
    }
    let mut matches = Vec::new();
    for entry in fs::read_dir(&admin).map_err(|error| WorktreeError::Unsafe(error.to_string()))? {
        let entry = entry.map_err(|error| WorktreeError::Unsafe(error.to_string()))?;
        if !entry
            .file_type()
            .map_err(|error| WorktreeError::Unsafe(error.to_string()))?
            .is_dir()
        {
            continue;
        }
        let gitdir = entry.path().join("gitdir");
        let Some(target) = read_gitdir(&gitdir)? else {
            continue;
        };
        let candidate = target
            .parent()
            .ok_or_else(|| WorktreeError::Malformed("Git worktree gitdir has no parent".into()))?;
        if same_path(candidate, path) {
            matches.push(path_string(&entry.path(), "Git worktree admin directory")?);
        }
    }
    match matches.as_slice() {
        [value] => Ok(value.clone()),
        [] => Err(WorktreeError::Malformed(format!(
            "no Git metadata identifies worktree {}",
            path.display()
        ))),
        _ => Err(WorktreeError::Malformed(format!(
            "ambiguous Git metadata identifies worktree {}",
            path.display()
        ))),
    }
}

fn read_gitdir(path: &Path) -> Result<Option<PathBuf>, WorktreeError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(WorktreeError::Unsafe(error.to_string())),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(WorktreeError::Unsafe(format!(
            "unsafe Git worktree metadata file {}",
            path.display()
        )));
    }
    let text =
        String::from_utf8(bounded_read(path, 4096)?).map_err(|_| WorktreeError::NonUtf8 {
            directory: path.to_owned(),
        })?;
    let line = text.lines().next().unwrap_or_default();
    let value = line.strip_prefix("gitdir: ").unwrap_or(line);
    if value.is_empty() || value.contains('\0') {
        return Err(WorktreeError::Malformed(format!(
            "invalid Git worktree gitdir file {}",
            path.display()
        )));
    }
    Ok(Some(absolute_path(
        path.parent().unwrap_or(Path::new(".")),
        Path::new(value),
    )))
}

fn parse_worktrees(bytes: &[u8]) -> Result<Vec<GitWorktree>, WorktreeError> {
    let mut records = Vec::new();
    let mut fields = Vec::new();
    for raw in bytes.split(|byte| *byte == 0) {
        if raw.is_empty() {
            if fields.is_empty() {
                continue;
            }
            records.push(parse_worktree_record(&fields)?);
            fields.clear();
        } else {
            fields.push(
                std::str::from_utf8(raw)
                    .map_err(|_| WorktreeError::Malformed("worktree list is not UTF-8".into()))?
                    .to_owned(),
            );
        }
    }
    if !fields.is_empty() {
        records.push(parse_worktree_record(&fields)?);
    }
    if records.is_empty() {
        return Err(WorktreeError::Malformed(
            "Git returned no worktree records".into(),
        ));
    }
    if records.len() > MAX_WORKTREE_RECORDS {
        return Err(WorktreeError::Malformed("too many Git worktrees".into()));
    }
    Ok(records)
}

fn parse_worktree_record(fields: &[String]) -> Result<GitWorktree, WorktreeError> {
    let mut path = None;
    let mut head = None;
    let mut branch = None;
    let mut detached = false;
    let mut prunable = false;
    let mut locked = false;
    for field in fields {
        if let Some(value) = field.strip_prefix("worktree ") {
            if path.replace(PathBuf::from(value)).is_some() {
                return Err(WorktreeError::Malformed("duplicate worktree path".into()));
            }
        } else if let Some(value) = field.strip_prefix("HEAD ") {
            validate_head(value)?;
            head = Some(value.to_owned());
        } else if let Some(value) = field.strip_prefix("branch ") {
            if value.is_empty() || !value.starts_with("refs/") {
                return Err(WorktreeError::Malformed(
                    "invalid worktree branch ref".into(),
                ));
            }
            branch = Some(value.to_owned());
        } else if field == "detached" {
            detached = true;
        } else if field == "locked" || field.starts_with("locked ") {
            locked = true;
        } else if field == "prunable" || field.starts_with("prunable ") {
            prunable = true;
        } else {
            return Err(WorktreeError::Malformed(format!(
                "unknown worktree field {field:?}"
            )));
        }
    }
    let path =
        path.ok_or_else(|| WorktreeError::Malformed("worktree record has no path".into()))?;
    if detached && branch.is_some() {
        return Err(WorktreeError::Malformed(
            "detached worktree has a branch".into(),
        ));
    }
    Ok(GitWorktree {
        path,
        head,
        branch,
        detached,
        prunable,
        locked,
    })
}

fn provenance(
    repository_id: PathBuf,
    repository_root: PathBuf,
    worktree_id: PathBuf,
    worktree_path: PathBuf,
    state: WorktreeState,
) -> Result<WorktreeProvenance, WorktreeError> {
    let result = WorktreeProvenance {
        schema_version: WORKTREE_SCHEMA_VERSION,
        repository_id: path_string(&repository_id, "repository identity")?,
        repository_root: path_string(&repository_root, "repository root")?,
        worktree_id: path_string(&worktree_id, "worktree identity")?,
        worktree_path: path_string(&worktree_path, "worktree path")?,
        branch: state.branch.map(|value| {
            value
                .strip_prefix("refs/heads/")
                .unwrap_or(&value)
                .to_owned()
        }),
        ref_name: state.ref_name,
        head: state.head,
        detached: state.detached,
    };
    validate_provenance(&result)?;
    Ok(result)
}

fn validate_provenance(value: &WorktreeProvenance) -> Result<(), WorktreeError> {
    if value.schema_version != WORKTREE_SCHEMA_VERSION {
        return Err(WorktreeError::Malformed(format!(
            "unsupported worktree schema {}",
            value.schema_version
        )));
    }
    for (name, item) in [
        ("repository_id", &value.repository_id),
        ("repository_root", &value.repository_root),
        ("worktree_id", &value.worktree_id),
        ("worktree_path", &value.worktree_path),
    ] {
        validate_id(item, name)?;
        if !Path::new(item).is_absolute() {
            return Err(WorktreeError::Malformed(format!("{name} must be absolute")));
        }
    }
    if let Some(head) = &value.head {
        validate_head(head)?;
    }
    if value.detached && value.branch.is_some() {
        return Err(WorktreeError::Malformed(
            "detached worktree has a branch".into(),
        ));
    }
    Ok(())
}

fn read_coordination(
    common: &Path,
    repository_id: &str,
) -> Result<CoordinationFile, WorktreeError> {
    let path = common.join(CLAIMS_FILE);
    let mut file = if path.exists() {
        let value: CoordinationFile =
            serde_json::from_slice(&bounded_read(&path, MAX_METADATA_BYTES)?).map_err(|error| {
                WorktreeError::Malformed(format!("{}: {error}", path.display()))
            })?;
        value
    } else {
        CoordinationFile {
            schema_version: WORKTREE_SCHEMA_VERSION,
            repository_id: repository_id.to_owned(),
            claims: Vec::new(),
            worktrees: Vec::new(),
        }
    };
    let history_path = common.join(CATALOG_FILE);
    if history_path.exists() {
        let history: CoordinationFile =
            serde_json::from_slice(&bounded_read(&history_path, MAX_METADATA_BYTES)?).map_err(
                |error| WorktreeError::Malformed(format!("{}: {error}", history_path.display())),
            )?;
        if history.schema_version != file.schema_version
            || history.repository_id != file.repository_id
            || !history.claims.is_empty()
        {
            return Err(WorktreeError::Malformed(format!(
                "invalid coordination history in {}",
                history_path.display()
            )));
        }
        file.worktrees = history.worktrees;
    }
    if file.schema_version != WORKTREE_SCHEMA_VERSION || file.repository_id != repository_id {
        return Err(WorktreeError::Malformed(format!(
            "invalid coordination metadata in {}",
            path.display()
        )));
    }
    if file.claims.len() > MAX_WORKTREE_RECORDS || file.worktrees.len() > MAX_WORKTREE_RECORDS {
        return Err(WorktreeError::Malformed(format!(
            "coordination metadata exceeds bounds in {}",
            path.display()
        )));
    }
    for worktree in &file.worktrees {
        validate_provenance(worktree)?;
    }
    Ok(file)
}

fn write_coordination(common: &Path, file: &CoordinationFile) -> Result<(), WorktreeError> {
    if file.claims.len() > MAX_WORKTREE_RECORDS || file.worktrees.len() > MAX_WORKTREE_RECORDS {
        return Err(WorktreeError::Malformed(
            "coordination metadata exceeds bounds".into(),
        ));
    }
    let claims_path = common.join(CLAIMS_FILE);
    let history_path = common.join(CATALOG_FILE);
    let mut claims = file.clone();
    claims.worktrees.clear();
    storage::write_json_atomic(&claims_path, &claims)?;
    let mut history = file.clone();
    history.claims.clear();
    storage::write_json_atomic(&history_path, &history)?;
    Ok(())
}

fn with_lock<T>(
    common: &Path,
    operation: impl FnOnce(&Path) -> Result<T, WorktreeError>,
) -> Result<T, WorktreeError> {
    ensure_directory(common)?;
    let lock_path = common.join(LOCK_FILE);
    reject_symlink(&lock_path)?;
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = options.open(&lock_path).map_err(|error| {
        WorktreeError::Unsafe(format!("could not open {}: {error}", lock_path.display()))
    })?;
    file.lock_exclusive().map_err(|error| {
        WorktreeError::Unsafe(format!("could not lock {}: {error}", lock_path.display()))
    })?;
    let result = operation(common);
    let _ = file.unlock();
    result
}

fn ensure_directory(path: &Path) -> Result<(), WorktreeError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        WorktreeError::Unsafe(format!("could not inspect {}: {error}", path.display()))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(WorktreeError::Unsafe(format!(
            "Git common directory is not a real directory: {}",
            path.display()
        )));
    }
    Ok(())
}

fn reject_symlink(path: &Path) -> Result<(), WorktreeError> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && metadata.file_type().is_symlink()
    {
        return Err(WorktreeError::Unsafe(format!(
            "symlinked coordination path {}",
            path.display()
        )));
    }
    Ok(())
}

fn repository_root(directory: &Path) -> Result<Option<PathBuf>, WorktreeError> {
    let output = run(directory, &["rev-parse", "--show-toplevel"])?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(absolute_path(
        directory,
        Path::new(&text(output.stdout, directory)?),
    )))
}

fn metadata_path(directory: &Path, args: &[&str]) -> Result<PathBuf, WorktreeError> {
    let value = text(run(directory, args)?.stdout, directory)?;
    let path = absolute_path(directory, Path::new(&value));
    canonical(&path, "Git metadata path")
}

fn optional_text(directory: &Path, args: &[&str]) -> Result<Option<String>, WorktreeError> {
    let output = run(directory, args)?;
    if output.status.success() {
        Ok(Some(text(output.stdout, directory)?))
    } else {
        Ok(None)
    }
}

fn run(directory: &Path, args: &[&str]) -> Result<Output, WorktreeError> {
    Command::new("git")
        .args(args)
        .current_dir(directory)
        .output()
        .map_err(|source| WorktreeError::Spawn {
            directory: directory.to_owned(),
            source,
        })
}

fn command_error(directory: &Path, output: Output) -> WorktreeError {
    WorktreeError::Command {
        directory: directory.to_owned(),
        message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    }
}

fn text(bytes: Vec<u8>, directory: &Path) -> Result<String, WorktreeError> {
    String::from_utf8(bytes)
        .map(|value| value.trim_end().to_owned())
        .map_err(|_| WorktreeError::NonUtf8 {
            directory: directory.to_owned(),
        })
}

fn canonical(path: &Path, name: &str) -> Result<PathBuf, WorktreeError> {
    reject_symlink(path)?;
    fs::canonicalize(path).map_err(|error| {
        WorktreeError::Unsafe(format!(
            "could not resolve {name} {}: {error}",
            path.display()
        ))
    })
}

fn absolute_path(base: &Path, path: &Path) -> PathBuf {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        base.join(path)
    };
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::Prefix(_) => result.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            Component::Normal(value) => result.push(value),
        }
    }
    result
}

fn same_path(left: &Path, right: &Path) -> bool {
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => absolute_path(Path::new("."), left) == absolute_path(Path::new("."), right),
    }
}

fn path_string(path: &Path, name: &str) -> Result<String, WorktreeError> {
    let value = path
        .to_str()
        .ok_or_else(|| WorktreeError::Malformed(format!("{name} is not UTF-8")))?;
    validate_id(value, name)?;
    Ok(value.to_owned())
}

fn validate_head(value: &str) -> Result<(), WorktreeError> {
    if !(4..=128).contains(&value.len()) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(WorktreeError::Malformed(
            "worktree HEAD is not a hexadecimal object id".into(),
        ));
    }
    Ok(())
}
fn validate_id(value: &str, name: &str) -> Result<(), WorktreeError> {
    if value.is_empty()
        || value.len() > MAX_ID_BYTES
        || value.contains('\0')
        || value.contains('\n')
    {
        return Err(WorktreeError::Malformed(format!(
            "{name} is empty or exceeds bounds"
        )));
    }
    Ok(())
}

fn bounded_read(path: &Path, maximum: u64) -> Result<Vec<u8>, WorktreeError> {
    reject_symlink(path)?;
    let metadata = fs::metadata(path).map_err(|error| {
        WorktreeError::Unsafe(format!("could not inspect {}: {error}", path.display()))
    })?;
    if !metadata.is_file() || metadata.len() > maximum {
        return Err(WorktreeError::Malformed(format!(
            "metadata file exceeds bounds: {}",
            path.display()
        )));
    }
    let bytes = fs::read(path).map_err(|error| {
        WorktreeError::Unsafe(format!("could not read {}: {error}", path.display()))
    })?;
    if bytes.len() as u64 > maximum {
        return Err(WorktreeError::Malformed(format!(
            "metadata file exceeds bounds: {}",
            path.display()
        )));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_detached_and_branch_records() {
        let records = parse_worktrees(b"worktree /repo\0HEAD abcdef0123456789\0branch refs/heads/main\0\0worktree /other\0HEAD abcdef0123456789\0detached\0\0").unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].branch.as_deref(), Some("refs/heads/main"));
        assert!(records[1].detached);
    }

    #[test]
    fn rejects_unknown_or_ambiguous_fields() {
        assert!(parse_worktrees(b"worktree /repo\0HEAD abcdef0123456789\0unknown\0\0").is_err());
        assert!(
            parse_worktrees(
                b"worktree /repo\0HEAD abcdef0123456789\0branch refs/heads/main\0detached\0\0"
            )
            .is_err()
        );
    }
}
