//! Bounded, startup-frozen discovery and lazy loading of local `SKILL.md` files.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::util;

pub const FRONTMATTER_MAX_BYTES: usize = 8 * 1024;
pub const SOURCE_MAX_BYTES: u64 = 1024 * 1024;
pub const BODY_MAX_CHARS: usize = 20_000;
pub const CATALOG_MAX_CHARS: usize = 24_000;
pub const MAX_DEPTH: usize = 4;
pub const MAX_ENTRIES: usize = 4_096;
pub const MAX_SKILLS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SkillScope {
    Repo,
    User,
}

impl SkillScope {
    pub fn as_str(self) -> &'static str {
        match self {
            SkillScope::Repo => "repo",
            SkillScope::User => "user",
        }
    }

    fn rank(self) -> u8 {
        match self {
            SkillScope::Repo => 0,
            SkillScope::User => 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub scope: SkillScope,
    pub path: PathBuf,
    pub content_hash: [u8; 32],
}

#[derive(Debug, Clone)]
pub struct SkillCatalog {
    pub ordered: Vec<SkillMetadata>,
    by_name: BTreeMap<String, usize>,
    roots: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct DiscoveryReport {
    pub catalog: SkillCatalog,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedSkill {
    pub metadata: SkillMetadata,
    pub body: String,
}

pub fn render_loaded_skill(skill: &LoadedSkill) -> String {
    format!(
        "<skill name=\"{}\" scope=\"{}\" path=\"{}\">\n{}\n</skill>",
        escape_xml(&skill.metadata.name),
        skill.metadata.scope.as_str(),
        escape_xml(&skill.metadata.path.display().to_string()),
        util::truncate_middle(&escape_xml(&skill.body), BODY_MAX_CHARS),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillLoadError {
    NotFound(String),
    Stale(String),
    Io(String),
    Invalid(String),
}

impl fmt::Display for SkillLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SkillLoadError::NotFound(message)
            | SkillLoadError::Stale(message)
            | SkillLoadError::Io(message)
            | SkillLoadError::Invalid(message) => f.write_str(message),
        }
    }
}

impl SkillCatalog {
    pub fn empty() -> Self {
        SkillCatalog {
            ordered: Vec::new(),
            by_name: BTreeMap::new(),
            roots: Vec::new(),
        }
    }

    pub fn get(&self, name: &str) -> Option<&SkillMetadata> {
        self.by_name
            .get(name)
            .and_then(|index| self.ordered.get(*index))
    }

    pub fn render_prompt(&self) -> String {
        if self.ordered.is_empty() {
            return String::new();
        }
        for description_chars in [512, 96, 0] {
            let mut out = String::from("<available_skills>\n");
            for skill in &self.ordered {
                let line = if description_chars == 0 {
                    format!("  <skill name=\"{}\" />\n", escape_xml(&skill.name))
                } else {
                    format!(
                        "  <skill name=\"{}\" scope=\"{}\" path=\"{}\" description=\"{}\" />\n",
                        escape_xml(&skill.name),
                        skill.scope.as_str(),
                        escape_xml(&util::ellipsis(&skill.path.display().to_string(), 256)),
                        escape_xml(&util::ellipsis(&skill.description, description_chars)),
                    )
                };
                out.push_str(&line);
            }
            out.push_str("</available_skills>");
            if out.chars().count() <= CATALOG_MAX_CHARS || description_chars == 0 {
                return out;
            }
        }
        unreachable!()
    }

    pub fn load(&self, name: &str) -> Result<LoadedSkill, SkillLoadError> {
        let metadata = self
            .get(name)
            .ok_or_else(|| {
                SkillLoadError::NotFound(format!("技能 {:?} 不在启动时发现的 catalog 中", name))
            })?
            .clone();
        let canonical = std::fs::canonicalize(&metadata.path).map_err(|error| {
            SkillLoadError::Stale(format!("技能 {:?} 文件已消失: {}", name, error))
        })?;
        if canonical != metadata.path || !self.roots.iter().any(|root| canonical.starts_with(root))
        {
            return Err(SkillLoadError::Stale(format!(
                "技能 {:?} 路径已脱离启动时批准的目录",
                name
            )));
        }
        let bytes = std::fs::read(&canonical)
            .map_err(|error| SkillLoadError::Io(format!("读取技能 {:?} 失败: {}", name, error)))?;
        if bytes.len() as u64 > SOURCE_MAX_BYTES {
            return Err(SkillLoadError::Invalid(format!(
                "技能 {:?} 文件超过 {} bytes 上限",
                name, SOURCE_MAX_BYTES
            )));
        }
        let hash = hash_bytes(&bytes);
        if hash != metadata.content_hash {
            return Err(SkillLoadError::Stale(format!(
                "技能 {:?} 内容已变化，请重新启动会话",
                name
            )));
        }
        let text = String::from_utf8(bytes)
            .map_err(|_| SkillLoadError::Invalid(format!("技能 {:?} 不是 UTF-8 文本", name)))?;
        let (_, body) = parse_document(&text)
            .map_err(|error| SkillLoadError::Invalid(format!("技能 {:?}: {}", name, error)))?;
        Ok(LoadedSkill { metadata, body })
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.ordered.iter().map(|skill| skill.name.as_str())
    }
}

pub fn discover(repo_root: &Path, user_root: &Path) -> DiscoveryReport {
    let mut candidates = Vec::new();
    let mut warnings = Vec::new();
    let mut roots = Vec::new();
    for (scope, root) in [(SkillScope::Repo, repo_root), (SkillScope::User, user_root)] {
        let Ok(canonical_root) = std::fs::canonicalize(root) else {
            continue;
        };
        if !canonical_root.is_dir() {
            continue;
        }
        roots.push(canonical_root.clone());
        let mut visited = 0;
        scan_root(
            &canonical_root,
            scope,
            0,
            &mut visited,
            &mut candidates,
            &mut warnings,
        );
    }
    candidates.sort_by(|left, right| {
        (left.scope.rank(), &left.name, &left.path).cmp(&(
            right.scope.rank(),
            &right.name,
            &right.path,
        ))
    });

    let mut by_path = BTreeMap::<PathBuf, Candidate>::new();
    for candidate in candidates {
        by_path
            .entry(candidate.path.clone())
            .and_modify(|existing| {
                if candidate.scope.rank() < existing.scope.rank() {
                    *existing = candidate.clone();
                }
            })
            .or_insert(candidate);
    }
    let mut deduplicated = by_path.into_values().collect::<Vec<_>>();
    deduplicated.sort_by(|left, right| {
        (left.scope.rank(), &left.name, &left.path).cmp(&(
            right.scope.rank(),
            &right.name,
            &right.path,
        ))
    });
    let mut winners = BTreeMap::<String, Candidate>::new();
    for candidate in deduplicated {
        if winners.contains_key(&candidate.name) {
            warnings.push(format!(
                "技能 {:?} 存在同名冲突，已按 scope/path 忽略 {}",
                candidate.name,
                candidate.path.display()
            ));
            continue;
        }
        if winners.len() >= MAX_SKILLS {
            warnings.push(format!("技能数量超过 {} 上限，后续条目已忽略", MAX_SKILLS));
            break;
        }
        winners.insert(candidate.name.clone(), candidate);
    }

    let mut ordered = winners
        .into_values()
        .map(|candidate| candidate.metadata())
        .collect::<Vec<_>>();
    ordered.sort_by(|left, right| {
        (left.scope.rank(), &left.name, &left.path).cmp(&(
            right.scope.rank(),
            &right.name,
            &right.path,
        ))
    });
    let by_name = ordered
        .iter()
        .enumerate()
        .map(|(index, skill)| (skill.name.clone(), index))
        .collect();
    DiscoveryReport {
        catalog: SkillCatalog {
            ordered,
            by_name,
            roots,
        },
        warnings,
    }
}

#[derive(Debug, Clone)]
struct Candidate {
    name: String,
    scope: SkillScope,
    path: PathBuf,
    description: String,
    content_hash: [u8; 32],
}

impl Candidate {
    fn metadata(self) -> SkillMetadata {
        SkillMetadata {
            name: self.name,
            description: self.description,
            scope: self.scope,
            path: self.path,
            content_hash: self.content_hash,
        }
    }
}

fn scan_root(
    root: &Path,
    scope: SkillScope,
    depth: usize,
    visited: &mut usize,
    candidates: &mut Vec<Candidate>,
    warnings: &mut Vec<String>,
) {
    if depth > MAX_DEPTH || candidates.len() >= MAX_SKILLS {
        return;
    }
    let mut entries = match std::fs::read_dir(root) {
        Ok(entries) => entries.filter_map(Result::ok).collect::<Vec<_>>(),
        Err(error) => {
            warnings.push(format!("扫描技能目录 {} 失败: {}", root.display(), error));
            return;
        }
    };
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        if *visited >= MAX_ENTRIES {
            if *visited == MAX_ENTRIES {
                warnings.push(format!(
                    "技能目录 {} 超过 {} entries 上限",
                    root.display(),
                    MAX_ENTRIES
                ));
            }
            break;
        }
        *visited += 1;
        if candidates.len() >= MAX_SKILLS {
            break;
        }
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            warnings.push(format!("无法读取技能目录项 {}", path.display()));
            continue;
        };
        if file_type.is_dir() {
            if file_type.is_symlink() {
                continue;
            }
            scan_root(&path, scope, depth + 1, visited, candidates, warnings);
            continue;
        }
        if !file_type.is_file()
            || path.file_name().and_then(|name| name.to_str()) != Some("SKILL.md")
        {
            continue;
        }
        let canonical = match std::fs::canonicalize(&path) {
            Ok(path) if path.starts_with(root) => path,
            Ok(_) => {
                warnings.push(format!("技能路径 {} 脱离批准目录，已忽略", path.display()));
                continue;
            }
            Err(error) => {
                warnings.push(format!(
                    "技能路径 {} 无法 canonicalize: {}",
                    path.display(),
                    error
                ));
                continue;
            }
        };
        match parse_candidate(&canonical, scope) {
            Ok(candidate) => candidates.push(candidate),
            Err(error) => warnings.push(format!("忽略技能 {}: {}", canonical.display(), error)),
        }
    }
}

fn parse_candidate(path: &Path, scope: SkillScope) -> Result<Candidate, String> {
    let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
    if bytes.len() as u64 > SOURCE_MAX_BYTES {
        return Err(format!("文件超过 {} bytes 上限", SOURCE_MAX_BYTES));
    }
    let text = String::from_utf8(bytes.clone()).map_err(|_| "不是 UTF-8 文本".to_string())?;
    let (frontmatter, _) = parse_document(&text)?;
    let name = frontmatter
        .get("name")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| valid_name(value))
        .ok_or_else(|| "frontmatter.name 缺失或非法".to_string())?
        .to_string();
    let description = frontmatter
        .get("description")
        .and_then(|value| value.as_str())
        .map(|value| util::ellipsis(&util::sanitize(value).replace(['\n', '\r'], " "), 1_024))
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "frontmatter.description 缺失或为空".to_string())?;
    Ok(Candidate {
        name,
        scope,
        path: path.to_path_buf(),
        description,
        content_hash: hash_bytes(&bytes),
    })
}

fn parse_document(text: &str) -> Result<(serde_yaml::Mapping, String), String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split_inclusive('\n');
    let first_segment = lines.next().unwrap_or("");
    let first = first_segment.trim_end_matches(['\r', '\n']);
    if first != "---" {
        return Err("必须以 --- frontmatter 开始".into());
    }
    let mut yaml = String::new();
    let mut body_start = 0usize;
    let mut consumed;
    let mut closed = false;
    consumed = first_segment.len();
    for line in lines {
        let marker = line.trim_end_matches(['\r', '\n']);
        consumed += line.len();
        if consumed > FRONTMATTER_MAX_BYTES {
            return Err(format!(
                "frontmatter 超过 {} bytes 上限",
                FRONTMATTER_MAX_BYTES
            ));
        }
        if marker == "---" || marker == "..." {
            body_start = consumed;
            closed = true;
            break;
        }
        yaml.push_str(line);
    }
    if !closed {
        return Err("frontmatter 缺少结束标记".into());
    }
    let value: serde_yaml::Value =
        serde_yaml::from_str(&yaml).map_err(|error| format!("YAML 解析失败: {}", error))?;
    let mapping = value
        .as_mapping()
        .cloned()
        .ok_or_else(|| "frontmatter 必须是 object".to_string())?;
    Ok((mapping, text[body_start.min(text.len())..].to_string()))
}

fn valid_name(name: &str) -> bool {
    if name.is_empty()
        || name.len() > 64
        || name.starts_with('-')
        || name.ends_with('-')
        || name.contains("--")
    {
        return false;
    }
    name.bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn hash_bytes(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_root(label: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("onemore-skills-{}-{}", label, uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_skill(root: &Path, dir: &str, name: &str, description: &str, body: &str) {
        let path = root.join(dir);
        fs::create_dir_all(&path).unwrap();
        fs::write(
            path.join("SKILL.md"),
            format!(
                "---\nname: {}\ndescription: {}\n---\n{}",
                name, description, body
            ),
        )
        .unwrap();
    }

    #[test]
    fn discovery_is_stable_and_repo_wins_same_name() {
        let repo = temp_root("repo");
        let user = temp_root("user");
        write_skill(&user, "shared", "shared", "user", "user");
        write_skill(&user, "zeta", "zeta", "user", "user");
        write_skill(&repo, "shared", "shared", "repo", "repo");
        write_skill(&repo, "alpha", "alpha", "a", "a");
        let report = discover(&repo, &user);
        assert_eq!(
            report.catalog.names().collect::<Vec<_>>(),
            vec!["alpha", "shared", "zeta"]
        );
        assert_eq!(
            report.catalog.get("shared").unwrap().scope,
            SkillScope::Repo
        );
        assert!(report
            .warnings
            .iter()
            .any(|warning| warning.contains("同名冲突")));
        let _ = fs::remove_dir_all(repo);
        let _ = fs::remove_dir_all(user);
    }

    #[test]
    fn malformed_or_unsafe_skills_only_warn() {
        let repo = temp_root("malformed");
        fs::create_dir_all(repo.join("bad")).unwrap();
        fs::write(repo.join("bad/SKILL.md"), "not frontmatter").unwrap();
        write_skill(&repo, "unsafe", "Bad_Name", "bad", "body");
        let report = discover(&repo, &temp_root("missing"));
        assert!(report.catalog.ordered.is_empty());
        assert_eq!(report.warnings.len(), 2);
        let _ = fs::remove_dir_all(repo);
    }

    #[test]
    fn load_rejects_hash_changes_and_escapes_body() {
        let repo = temp_root("load");
        write_skill(&repo, "demo", "demo", "desc", "<tag>&\nbody");
        let report = discover(&repo, &temp_root("missing"));
        let loaded = report.catalog.load("demo").unwrap();
        assert!(loaded.body.contains("<tag>"));
        fs::write(
            repo.join("demo/SKILL.md"),
            "---\nname: demo\ndescription: desc\n---\nchanged",
        )
        .unwrap();
        assert!(matches!(
            report.catalog.load("demo"),
            Err(SkillLoadError::Stale(_))
        ));
        let _ = fs::remove_dir_all(repo);
    }

    #[test]
    fn catalog_prompt_escapes_untrusted_metadata() {
        let repo = temp_root("escape");
        write_skill(&repo, "demo", "demo", "a & b", "body");
        let prompt = discover(&repo, &temp_root("missing"))
            .catalog
            .render_prompt();
        assert!(prompt.contains("a &amp; b"));
        let _ = fs::remove_dir_all(repo);
    }
}
