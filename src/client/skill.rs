//! Writes what agents read about roost: a skill (Claude Code, Pi) and a block in AGENTS.md.

use std::path::{Path, PathBuf};

use clap::{Subcommand, ValueEnum};
use serde_json::{Value, json};

use super::config::Config;
use super::{Exit, Res, print_json};

const SKILL: &str = include_str!("skill/SKILL.md");
const STAY_ONLINE: &str = include_str!("skill/stay-online.md");
const MARKER: &str = "roost-skill";
const BEGIN: &str = "<!-- roost-skill:begin -->";
const END: &str = "<!-- roost-skill:end -->";

#[derive(Subcommand)]
pub enum SkillCommand {
    /// Write the roost skill into this project, or for every project with --global (or update it).
    Install {
        /// claude: .claude/skills/roost/ · pi: .agents/skills/roost/ · agents: a block in AGENTS.md (Codex, Gemini and others).
        #[arg(long, value_enum, default_value = "claude")]
        target: Target,
        /// Install for this user instead: $CLAUDE_CONFIG_DIR/skills (default ~/.claude) or $PI_CODING_AGENT_DIR/skills (default ~/.pi/agent).
        #[arg(long)]
        global: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Target {
    Claude,
    Pi,
    Agents,
}

fn root(cfg: &Config) -> PathBuf {
    cfg.project_root
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default()
}

fn write(file: &Path, text: &str) -> Res<()> {
    let fail =
        |e: std::io::Error| Exit::error(format!("roost: can't write {}: {e}", file.display()));
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(fail)?;
    }
    let tmp = file.with_extension("roost-tmp");
    std::fs::write(&tmp, text).map_err(fail)?;
    std::fs::rename(&tmp, file).map_err(fail)
}

/// Writes `text` unless the file holds something that isn't ours, which is kept as `.bak` first.
fn replace_ours(file: &Path, text: &str) -> Res<()> {
    if let Ok(old) = std::fs::read_to_string(file)
        && !old.contains(MARKER)
    {
        let bak = file.with_extension("md.bak");
        std::fs::rename(file, &bak)
            .map_err(|e| Exit::error(format!("roost: can't back up {}: {e}", file.display())))?;
        eprintln!(
            "roost: kept your previous {} as {}",
            file.display(),
            bak.display()
        );
    }
    write(file, text)
}

/// The skill as one markdown section for AGENTS.md: no frontmatter, the reference inlined.
fn agents_block() -> String {
    let body = SKILL
        .split_once("\n---\n")
        .map_or(SKILL, |(_, rest)| rest)
        .replace("<!-- roost-skill-end -->", "")
        .replace(
            "Read `references/stay-online.md` first.",
            "Read \"Staying online\" below first.",
        );
    let online = STAY_ONLINE
        .replace("<!-- roost-skill-end -->", "")
        .replace("\n## ", "\n### ")
        .replacen("# Staying online", "## Staying online", 1);
    format!("{BEGIN}\n{}\n\n{}\n{END}\n", body.trim(), online.trim())
}

pub fn install_claude(cfg: &Config, global: bool) -> Res<PathBuf> {
    install_skill(&if global {
        super::claude_dir().join("skills/roost")
    } else {
        root(cfg).join(".claude/skills/roost")
    })
}

/// In a project Pi reads `.agents/skills/` (the shared Agent Skills location), not `.claude/`.
pub fn install_pi(cfg: &Config, global: bool) -> Res<PathBuf> {
    install_skill(&if global {
        super::pi_agent_dir().join("skills/roost")
    } else {
        root(cfg).join(".agents/skills/roost")
    })
}

fn install_skill(dir: &Path) -> Res<PathBuf> {
    let file = dir.join("SKILL.md");
    replace_ours(&file, SKILL)?;
    replace_ours(&dir.join("references/stay-online.md"), STAY_ONLINE)?;
    Ok(file)
}

pub enum Agents {
    Written(PathBuf),
    Updated(PathBuf),
    /// AGENTS.md exists without our block; `append` was false.
    Foreign(PathBuf),
}

pub fn install_agents(cfg: &Config, append: bool) -> Res<Agents> {
    let file = root(cfg).join("AGENTS.md");
    let block = agents_block();
    let Ok(old) = std::fs::read_to_string(&file) else {
        write(&file, &block)?;
        return Ok(Agents::Written(file));
    };
    if let (Some(start), Some(end)) = (old.find(BEGIN), old.find(END)) {
        let end = end + END.len();
        let tail = old[end..].strip_prefix('\n').unwrap_or(&old[end..]);
        write(&file, &format!("{}{block}{tail}", &old[..start]))?;
        return Ok(Agents::Updated(file));
    }
    if !append {
        return Ok(Agents::Foreign(file));
    }
    let sep = if old.ends_with("\n\n") {
        ""
    } else if old.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    write(&file, &format!("{old}{sep}{block}"))?;
    Ok(Agents::Updated(file))
}

pub fn command(cfg: &Config, cmd: SkillCommand) -> Res {
    let SkillCommand::Install { target, global } = cmd;
    let out: Value = match target {
        Target::Claude => {
            let file = install_claude(cfg, global)?;
            json!({ "ok": true, "target": "claude", "global": global, "file": file })
        }
        Target::Pi => {
            let file = install_pi(cfg, global)?;
            json!({ "ok": true, "target": "pi", "global": global, "file": file })
        }
        Target::Agents if global => {
            return Err(Exit::usage(
                "roost: --global works with --target claude or pi; AGENTS.md is per project",
            ));
        }
        Target::Agents => match install_agents(cfg, true)? {
            Agents::Written(f) | Agents::Updated(f) | Agents::Foreign(f) => {
                json!({ "ok": true, "target": "agents", "file": f })
            }
        },
    };
    print_json(&out);
    Ok(0)
}

/// The setup step: the Claude skill always, AGENTS.md only when it's absent or already ours.
/// roost's skill installed for the user, if any: setup then leaves the project without a second copy.
fn global_skill(dir: PathBuf) -> Option<PathBuf> {
    let file = dir.join("skills/roost/SKILL.md");
    std::fs::read_to_string(&file)
        .is_ok_and(|s| s.contains(MARKER))
        .then_some(file)
}

pub fn after_setup(cfg: &Config, note: &dyn Fn(&str)) {
    let pi_global = global_skill(super::pi_agent_dir());
    let claude_global = global_skill(super::claude_dir());
    if let Some(f) = &pi_global {
        note(&format!(
            "roost: Pi skill installed globally ({}); none written here",
            f.display()
        ));
    } else if super::in_pi() {
        match install_pi(cfg, false) {
            Ok(f) => note(&format!("roost: Pi skill written to {}", f.display())),
            Err(e) => eprintln!(
                "roost: Pi skill not written ({}); run `roost skill install --target pi` later",
                e.msg.unwrap_or_default()
            ),
        }
    }
    if let Some(f) = &claude_global {
        note(&format!(
            "roost: Claude Code skill installed globally ({}); none written here",
            f.display()
        ));
    } else {
        match install_claude(cfg, false) {
            Ok(f) => note(&format!(
                "roost: skill written to {}; future sessions here know roost",
                f.display()
            )),
            Err(e) => eprintln!(
                "roost: skill not written ({}); run `roost skill install` later",
                e.msg.unwrap_or_default()
            ),
        }
    }
    match install_agents(cfg, false) {
        Ok(Agents::Written(f)) => note(&format!(
            "roost: {} written for non-Claude agents",
            f.display()
        )),
        Ok(Agents::Updated(f)) => note(&format!("roost: {} updated", f.display())),
        Ok(Agents::Foreign(f)) => note(&format!(
            "roost: {} is the project's own; left untouched (`roost skill install --target agents` adds a roost section)",
            f.display()
        )),
        Err(e) => eprintln!(
            "roost: AGENTS.md not written ({})",
            e.msg.unwrap_or_default()
        ),
    }
}
