/// /init — one-shot project context priming.
///
/// Walks the project and generates `.parecode/conventions.md` from existing
/// project files. Zero model calls — pure deterministic text extraction.
///
/// Sources (priority order):
///   1. README.md — first 50 lines (project description, stack)
///   2. Cargo.toml / package.json / pyproject.toml / go.mod — name + key deps
///   3. AGENTS.md / CLAUDE.md — merge if already exists
///   4. .eslintrc / rustfmt.toml / pyproject.toml [tool.ruff] — style hints
///   5. Test file structure — infer test runner
use std::fs;
use std::path::Path;

/// Run project init in the current working directory.
/// Returns the generated conventions content as a String.
pub fn run_project_init(cwd: &Path) -> String {
    let mut out = String::new();

    // ── 1. Project name + description ─────────────────────────────────────────
    let (name, lang, deps, test_runner) = detect_project_manifest(cwd);
    out.push_str(&format!("# Project: {name}\n"));
    if !lang.is_empty() {
        out.push_str(&format!("Language: {lang}\n"));
    }
    if !test_runner.is_empty() {
        out.push_str(&format!("Test runner: `{test_runner}`\n"));
    }
    if !deps.is_empty() {
        out.push_str(&format!("Key dependencies: {deps}\n"));
    }

    // ── 2. Style rules ────────────────────────────────────────────────────────
    let style_hints = detect_style(cwd);
    if !style_hints.is_empty() {
        out.push('\n');
        out.push_str("## Style\n");
        for hint in &style_hints {
            out.push_str(&format!("- {hint}\n"));
        }
    }

    // ── 3. README excerpt ─────────────────────────────────────────────────────
    if let Some(readme) = read_first_lines(cwd, &["README.md", "readme.md", "Readme.md"], 50) {
        out.push('\n');
        out.push_str("## README (first 50 lines)\n");
        out.push_str(&readme);
        out.push('\n');
    }

    // ── 4. Merge existing AGENTS.md / CLAUDE.md if present ───────────────────
    for existing in &["AGENTS.md", "CLAUDE.md", ".parecode/conventions.md"] {
        let p = cwd.join(existing);
        if p.exists() {
            if let Ok(content) = fs::read_to_string(&p) {
                out.push('\n');
                out.push_str(&format!("## Existing conventions (from {existing})\n"));
                out.push_str(&content);
            }
            break; // only include the first one found
        }
    }

    out
}

/// Save the generated content to `.parecode/conventions.md`.
pub fn save_conventions(cwd: &Path, content: &str) -> anyhow::Result<std::path::PathBuf> {
    let parecode_dir = cwd.join(".parecode");
    fs::create_dir_all(&parecode_dir)?;
    let path = parecode_dir.join("conventions.md");
    fs::write(&path, content)?;
    Ok(path)
}

// ── Manifest detection ────────────────────────────────────────────────────────

/// Returns (project_name, language_desc, key_deps_oneliner, test_runner_cmd)
fn detect_project_manifest(cwd: &Path) -> (String, String, String, String) {
    // Rust — Cargo.toml
    let cargo = cwd.join("Cargo.toml");
    if cargo.exists() {
        if let Ok(raw) = fs::read_to_string(&cargo) {
            let name = toml_field(&raw, "name").unwrap_or_else(|| "rust-project".to_string());
            let lang = "Rust".to_string();
            let deps = extract_cargo_deps(&raw);
            let test_runner = "cargo test".to_string();
            return (name, lang, deps, test_runner);
        }
    }

    // Node.js — package.json
    let pkg = cwd.join("package.json");
    if pkg.exists() {
        if let Ok(raw) = fs::read_to_string(&pkg) {
            let name = json_field(&raw, "name").unwrap_or_else(|| "node-project".to_string());
            let lang = detect_node_runtime(cwd);
            let deps = extract_npm_deps(&raw);
            let test_runner = detect_node_test_runner(cwd, &raw);
            return (name, lang, deps, test_runner);
        }
    }

    // Python — pyproject.toml
    let pyproject = cwd.join("pyproject.toml");
    if pyproject.exists() {
        if let Ok(raw) = fs::read_to_string(&pyproject) {
            let name = toml_field(&raw, "name").unwrap_or_else(|| "python-project".to_string());
            let lang = "Python".to_string();
            let deps = extract_pyproject_deps(&raw);
            let test_runner = detect_python_test_runner(cwd);
            return (name, lang, deps, test_runner);
        }
    }

    // Go — go.mod
    let gomod = cwd.join("go.mod");
    if gomod.exists() {
        if let Ok(raw) = fs::read_to_string(&gomod) {
            let name = raw
                .lines()
                .find(|l| l.starts_with("module "))
                .map(|l| l.trim_start_matches("module ").trim().to_string())
                .unwrap_or_else(|| "go-project".to_string());
            return (name, "Go".to_string(), String::new(), "go test ./...".to_string());
        }
    }

    ("project".to_string(), String::new(), String::new(), String::new())
}

// ── Style detection ───────────────────────────────────────────────────────────

fn detect_style(cwd: &Path) -> Vec<String> {
    let mut hints = Vec::new();

    // rustfmt.toml
    if cwd.join("rustfmt.toml").exists() || cwd.join(".rustfmt.toml").exists() {
        hints.push("Rust: rustfmt enforced (run `cargo fmt` after edits)".to_string());
    }

    // .eslintrc variants
    for rc in &[".eslintrc", ".eslintrc.js", ".eslintrc.json", ".eslintrc.cjs", "eslint.config.js", "eslint.config.mjs"] {
        if cwd.join(rc).exists() {
            hints.push(format!("ESLint configured ({rc}) — run `eslint --fix` after edits"));
            break;
        }
    }

    // Prettier
    for pc in &[".prettierrc", ".prettierrc.json", ".prettierrc.js", "prettier.config.js"] {
        if cwd.join(pc).exists() {
            hints.push("Prettier configured — run `prettier --write` after edits".to_string());
            break;
        }
    }

    // ruff (Python)
    if let Ok(raw) = fs::read_to_string(cwd.join("pyproject.toml")) {
        if raw.contains("[tool.ruff]") {
            hints.push("Ruff linter configured — run `ruff check --fix` after edits".to_string());
        }
    }
    if cwd.join("ruff.toml").exists() || cwd.join(".ruff.toml").exists() {
        hints.push("Ruff linter configured — run `ruff check --fix` after edits".to_string());
    }

    // clippy
    if cwd.join(".cargo/config.toml").exists() {
        if let Ok(raw) = fs::read_to_string(cwd.join(".cargo/config.toml")) {
            if raw.contains("clippy") {
                hints.push("Clippy configured — run `cargo clippy` to check".to_string());
            }
        }
    }

    hints.dedup();
    hints
}

// ── File helpers ──────────────────────────────────────────────────────────────

fn read_first_lines(cwd: &Path, candidates: &[&str], n: usize) -> Option<String> {
    for name in candidates {
        let p = cwd.join(name);
        if p.exists() {
            if let Ok(content) = fs::read_to_string(&p) {
                let lines: Vec<&str> = content.lines().take(n).collect();
                return Some(lines.join("\n"));
            }
        }
    }
    None
}

// ── TOML/JSON field extractors ─────────────────────────────────────────────────

fn toml_field(content: &str, field: &str) -> Option<String> {
    let needle = format!("{field} = ");
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with(&needle) {
            let val = trimmed[needle.len()..].trim().trim_matches('"');
            return Some(val.to_string());
        }
    }
    None
}

fn json_field(content: &str, field: &str) -> Option<String> {
    // Simple line-by-line search for "field": "value"
    let needle = format!("\"{}\":", field);
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with(&needle) {
            let rest = &trimmed[needle.len()..].trim();
            let val = rest.trim_start_matches('"').trim_end_matches(['"', ','].as_ref());
            return Some(val.to_string());
        }
    }
    None
}

fn extract_cargo_deps(content: &str) -> String {
    // Collect lines after [dependencies] until next [section]
    let mut in_deps = false;
    let mut deps: Vec<String> = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == "[dependencies]" {
            in_deps = true;
            continue;
        }
        if in_deps {
            if trimmed.starts_with('[') {
                break;
            }
            if let Some(name) = trimmed.split('=').next() {
                let name = name.trim();
                if !name.is_empty() && !name.starts_with('#') {
                    deps.push(name.to_string());
                }
            }
        }
    }
    deps.truncate(8);
    deps.join(", ")
}

fn extract_npm_deps(content: &str) -> String {
    // Extract keys from "dependencies" object — naive line scan
    let mut in_deps = false;
    let mut deps: Vec<String> = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.contains("\"dependencies\"") {
            in_deps = true;
            continue;
        }
        if in_deps {
            if trimmed == "}" || trimmed == "}," {
                break;
            }
            if let Some(name) = trimmed.split(':').next() {
                let name = name.trim().trim_matches('"');
                if !name.is_empty() {
                    deps.push(name.to_string());
                }
            }
        }
    }
    deps.truncate(8);
    deps.join(", ")
}

fn extract_pyproject_deps(content: &str) -> String {
    let mut in_deps = false;
    let mut deps: Vec<String> = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == "dependencies = [" || trimmed.starts_with("dependencies = [") {
            in_deps = true;
            continue;
        }
        if in_deps {
            if trimmed == "]" {
                break;
            }
            let clean = trimmed.trim_matches(['"', '\'', ',', ' '].as_ref());
            // Strip version specifiers: "requests>=2.0" → "requests"
            let name = clean.split(['>', '<', '=', '!', '[']).next().unwrap_or(clean);
            if !name.is_empty() {
                deps.push(name.to_string());
            }
        }
    }
    deps.truncate(8);
    deps.join(", ")
}

fn detect_node_runtime(cwd: &Path) -> String {
    if cwd.join("bun.lockb").exists() || cwd.join("bun.lock").exists() {
        "TypeScript (Bun runtime)".to_string()
    } else if cwd.join("deno.json").exists() || cwd.join("deno.jsonc").exists() {
        "TypeScript (Deno runtime)".to_string()
    } else if cwd.join("tsconfig.json").exists() {
        "TypeScript (Node.js)".to_string()
    } else {
        "JavaScript (Node.js)".to_string()
    }
}

fn detect_node_test_runner(cwd: &Path, pkg_content: &str) -> String {
    if cwd.join("jest.config.js").exists()
        || cwd.join("jest.config.ts").exists()
        || cwd.join("jest.config.mjs").exists()
        || pkg_content.contains("\"jest\"")
    {
        "npx jest".to_string()
    } else if cwd.join("vitest.config.ts").exists()
        || cwd.join("vitest.config.js").exists()
        || pkg_content.contains("\"vitest\"")
    {
        "npx vitest".to_string()
    } else if cwd.join("bun.lockb").exists() || cwd.join("bun.lock").exists() {
        "bun test".to_string()
    } else {
        "npm test".to_string()
    }
}

fn detect_python_test_runner(cwd: &Path) -> String {
    if cwd.join("pytest.ini").exists()
        || cwd.join("setup.cfg").exists()
        || cwd.join("conftest.py").exists()
    {
        "pytest".to_string()
    } else {
        "python -m pytest".to_string()
    }
}
