//! smol new — scaffold a runnable project: app code, Smolfile, and a README
//! whose three commands take it from empty directory to deployed tool.
//!
//! Aimed at the person building their first internal tool, not at someone who
//! already has a repo (that's `smol file init`). The generated app is a tiny
//! working example of the thing this audience actually builds — a shared
//! list that replaces a spreadsheet — so the first `smol file up` shows their
//! own tool working, not a hello-world.

use clap::Args;
use std::path::{Path, PathBuf};

#[derive(Args, Debug)]
pub struct NewCmd {
    /// Directory to create (also the project name)
    #[arg(value_name = "NAME")]
    pub name: String,

    /// Project template: flask, node
    #[arg(long, default_value = "flask", value_name = "TEMPLATE")]
    pub template: String,
}

struct ProjectFile {
    path: &'static str,
    contents: &'static str,
}

impl NewCmd {
    pub fn run(self) -> anyhow::Result<()> {
        let dir = PathBuf::from(&self.name);
        if dir.exists() {
            anyhow::bail!("'{}' already exists", self.name);
        }

        let files: &[ProjectFile] = match self.template.as_str() {
            "flask" => FLASK_FILES,
            "node" => NODE_FILES,
            other => anyhow::bail!("unknown template: '{other}'. Available: flask, node"),
        };

        for file in files {
            let path = dir.join(file.path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, file.contents)?;
        }

        println!("Created {}/", self.name);
        for file in files {
            println!("  {}", file.path);
        }
        println!(
            "\nNext:\n  cd {}\n  smol file up      # run it locally\n  smol cloud deploy # put it online (private to you by default)",
            self.name
        );
        Ok(())
    }
}

/// Verify a template stays inside the project directory. Templates are
/// compile-time constants, so this is a guard on our own additions, not on
/// user input.
#[allow(dead_code)]
fn template_path_is_safe(path: &str) -> bool {
    let p = Path::new(path);
    p.is_relative() && !path.split('/').any(|c| c == "..")
}

const FLASK_FILES: &[ProjectFile] = &[
    ProjectFile {
        path: "app.py",
        contents: r#"# A tiny shared tracker — the spreadsheet replacement, as a starting point.
# Rows live in an SQLite file next to the app. Edit freely: this file is yours.

import sqlite3
from flask import Flask, g, redirect, render_template_string, request

app = Flask(__name__)
DB = "tracker.db"

PAGE = """
<!doctype html><meta charset="utf-8"><title>Tracker</title>
<style>
  body { font: 16px system-ui; max-width: 40rem; margin: 3rem auto; padding: 0 1rem; }
  li { margin: .4rem 0; } form { margin-top: 1.5rem; }
  input[type=text] { padding: .4rem; width: 70%; }
</style>
<h1>Tracker</h1>
<ul>
  {% for id, item in rows %}
    <li>{{ item }}
      <form method="post" action="/done/{{ id }}" style="display:inline;margin:0">
        <button>done</button>
      </form>
    </li>
  {% else %}
    <li><em>Nothing yet — add the first item below.</em></li>
  {% endfor %}
</ul>
<form method="post" action="/add">
  <input type="text" name="item" placeholder="New item" autofocus required>
  <button>Add</button>
</form>
"""


def db():
    if "db" not in g:
        g.db = sqlite3.connect(DB)
        g.db.execute("CREATE TABLE IF NOT EXISTS items (id INTEGER PRIMARY KEY, item TEXT)")
    return g.db


@app.teardown_appcontext
def close(_exc):
    if (conn := g.pop("db", None)) is not None:
        conn.close()


@app.get("/")
def index():
    rows = db().execute("SELECT id, item FROM items ORDER BY id").fetchall()
    return render_template_string(PAGE, rows=rows)


@app.post("/add")
def add():
    if item := request.form.get("item", "").strip():
        db().execute("INSERT INTO items (item) VALUES (?)", (item,))
        db().commit()
    return redirect("/")


@app.post("/done/<int:item_id>")
def done(item_id):
    db().execute("DELETE FROM items WHERE id = ?", (item_id,))
    db().commit()
    return redirect("/")


if __name__ == "__main__":
    app.run(host="0.0.0.0", port=8000)
"#,
    },
    ProjectFile {
        path: "requirements.txt",
        contents: "flask\n",
    },
    ProjectFile {
        path: "Smolfile",
        contents: r#"image = "python:3.12-slim"
workdir = "/app"
cpus = 2
memory = 1024
net = true
entrypoint = ["python", "app.py"]

[dev]
volumes = [".:/app"]
ports = ["8000:8000"]
init = ["pip install -r requirements.txt"]
"#,
    },
    ProjectFile {
        path: "README.md",
        contents: r#"# Tracker

A tiny shared tracker — a starting point for replacing a spreadsheet with a
real tool. The app is `app.py`; edit it freely (or have your AI assistant do
it) and refresh.

## Run it locally

```sh
smol file up
```

Then open http://localhost:8000. The machine it runs in has its own isolated
filesystem and network; your laptop stays clean.

## Put it online

```sh
smol cloud deploy
```

The deployed app is **private by default** — only you can open it until you
deliberately share or publish it. Data written next to the app persists across
restarts.

## Where things live

- `app.py` — the whole app. One file on purpose.
- `Smolfile` — how it runs: image, resources, ports. Committed with the code,
  so "how this deploys" is reviewable like everything else.
"#,
    },
];

const NODE_FILES: &[ProjectFile] = &[
    ProjectFile {
        path: "server.js",
        contents: r#"// A tiny shared tracker — the spreadsheet replacement, as a starting point.
// Rows live in tracker.json next to the app. Edit freely: this file is yours.

const http = require("http");
const fs = require("fs");
const { parse } = require("querystring");

const DB = "tracker.json";
const load = () => (fs.existsSync(DB) ? JSON.parse(fs.readFileSync(DB, "utf8")) : []);
const save = (rows) => fs.writeFileSync(DB, JSON.stringify(rows, null, 2));

const esc = (s) =>
  s.replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);

const page = (rows) => `<!doctype html><meta charset="utf-8"><title>Tracker</title>
<style>
  body { font: 16px system-ui; max-width: 40rem; margin: 3rem auto; padding: 0 1rem; }
  li { margin: .4rem 0; } form { margin-top: 1.5rem; }
  input[type=text] { padding: .4rem; width: 70%; }
</style>
<h1>Tracker</h1>
<ul>${
  rows.length
    ? rows
        .map(
          (r, i) =>
            `<li>${esc(r)} <form method="post" action="/done/${i}" style="display:inline;margin:0"><button>done</button></form></li>`
        )
        .join("")
    : "<li><em>Nothing yet — add the first item below.</em></li>"
}</ul>
<form method="post" action="/add">
  <input type="text" name="item" placeholder="New item" autofocus required>
  <button>Add</button>
</form>`;

const body = (req) =>
  new Promise((resolve) => {
    let data = "";
    req.on("data", (c) => (data += c));
    req.on("end", () => resolve(parse(data)));
  });

http
  .createServer(async (req, res) => {
    const rows = load();
    if (req.method === "POST" && req.url === "/add") {
      const { item } = await body(req);
      if (item && item.trim()) rows.push(item.trim());
      save(rows);
      res.writeHead(303, { location: "/" }).end();
    } else if (req.method === "POST" && req.url.startsWith("/done/")) {
      rows.splice(Number(req.url.slice(6)), 1);
      save(rows);
      res.writeHead(303, { location: "/" }).end();
    } else {
      res.writeHead(200, { "content-type": "text/html" }).end(page(rows));
    }
  })
  .listen(3000, "0.0.0.0", () => console.log("listening on :3000"));
"#,
    },
    ProjectFile {
        path: "package.json",
        contents: r#"{
  "name": "tracker",
  "private": true,
  "scripts": { "start": "node server.js" }
}
"#,
    },
    ProjectFile {
        path: "Smolfile",
        contents: r#"image = "node:22-slim"
workdir = "/app"
cpus = 2
memory = 1024
net = true
entrypoint = ["node", "server.js"]

[dev]
volumes = [".:/app"]
ports = ["3000:3000"]
"#,
    },
    ProjectFile {
        path: "README.md",
        contents: r#"# Tracker

A tiny shared tracker — a starting point for replacing a spreadsheet with a
real tool. The app is `server.js`; edit it freely (or have your AI assistant
do it) and refresh.

## Run it locally

```sh
smol file up
```

Then open http://localhost:3000. The machine it runs in has its own isolated
filesystem and network; your laptop stays clean.

## Put it online

```sh
smol cloud deploy
```

The deployed app is **private by default** — only you can open it until you
deliberately share or publish it. Data written next to the app persists across
restarts.

## Where things live

- `server.js` — the whole app. One file on purpose, no dependencies.
- `Smolfile` — how it runs: image, resources, ports. Committed with the code,
  so "how this deploys" is reviewable like everything else.
"#,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_template_paths_stay_inside_the_project() {
        for file in FLASK_FILES.iter().chain(NODE_FILES) {
            assert!(
                template_path_is_safe(file.path),
                "unsafe template path: {}",
                file.path
            );
        }
    }

    #[test]
    fn smolfiles_in_templates_parse() {
        for file in FLASK_FILES.iter().chain(NODE_FILES) {
            if file.path == "Smolfile" {
                toml::from_str::<toml::Value>(file.contents)
                    .expect("Smolfile template is valid TOML");
            }
        }
    }
}
