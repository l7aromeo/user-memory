#![allow(
    clippy::unwrap_used,
    reason = "integration tests may unwrap: a failed unwrap is a failed test"
)]

#[path = "support/sandbox.rs"]
mod sandbox;
use sandbox::Sandboxed;
#[path = "support/serve.rs"]
mod serve;
#[path = "support/time.rs"]
mod time;

use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use tempfile::tempdir;

/// A `user-memory serve` in `folder`, over the store in `home`.
/// An empty git config file in `dir`, for GIT_CONFIG_GLOBAL: the user's own
/// git settings never reach a test, and a file works on every platform,
/// where the null device's name differs.
fn empty_git_config(dir: &Path) -> PathBuf {
    let file = dir.join(".gitconfig-empty");
    if !file.exists() {
        std::fs::write(&file, "").unwrap();
    }
    file
}

fn server(home: &Path, folder: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_user-memory"));
    command
        .arg("serve")
        .current_dir(folder)
        .sandboxed(home)
        .env("USER_MEMORY_DB", home.join("memory.sqlite3"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", empty_git_config(home))
        .env("GIT_CEILING_DIRECTORIES", home)
        .env_remove("USER_MEMORY_URL")
        .env_remove("USER_MEMORY_TOKEN_FILE");
    command
}

/// A git repository `name` in `home` whose origin remote is `remote`.
fn repository(home: &Path, name: &str, remote: &str) -> PathBuf {
    let repo = home.join(name);
    std::fs::create_dir(&repo).unwrap();
    for args in [&["init", "-q"][..], &["remote", "add", "origin", remote]] {
        let status = Command::new("git")
            .args(args)
            .current_dir(&repo)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", empty_git_config(home))
            .status()
            .unwrap();
        assert!(status.success());
    }
    repo
}

#[test]
fn initialization_delivers_portable_memory_guidance_and_keeps_tools_available() {
    let tmp = tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_user-memory"))
        .arg("serve")
        .sandboxed(tmp.path())
        .env("USER_MEMORY_DB", tmp.path().join("memory.sqlite3"))
        .env_remove("USER_MEMORY_URL")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let messages = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"integration-test","version":"1"}}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    ];
    for msg in messages {
        writeln!(input, "{msg}").unwrap();
        input.flush().unwrap();
    }
    let mut responses = Vec::new();
    while responses.len() < 2 {
        let mut line = String::new();
        assert!(output.read_line(&mut line).unwrap() > 0);
        let msg: Value = serde_json::from_str(&line).unwrap();
        if msg.get("id").is_some() {
            responses.push(msg);
        }
    }
    drop(input);
    child.wait().unwrap();
    let init = responses.iter().find(|m| m["id"] == 1).unwrap();
    let guidance = init["result"]["instructions"]
        .as_str()
        .expect("MCP initialization must carry memory workflow instructions");
    for rule in [
        "index",
        "search",
        "user",
        "reason",
        "tentative",
        "forget",
        "confirm it rather than recording it again",
        "follow the user-memory skill when it is available",
    ] {
        assert!(guidance.contains(rule), "Missing guidance for {rule}");
    }
    // What user-memory is comes first, then the check before any work, which
    // also covers clients that load MCP tools only on demand.
    assert!(guidance.starts_with("user-memory is this user's long-term memory: "));
    assert!(guidance.contains(
        "Before starting any task, check this user's memory. If the user-memory index is not already in your context from session start, call index first; if your client has not loaded the user-memory tools yet, load them first. Working in another repository this session? Call link with projects: [<its git remote>]; if this folder always needs it, list it (user-memory skill). Each index line is a pointer, not the full memory. When a memory applies to what you are doing, read it first and follow its full text (conditions, exceptions, reason): read with its ids, all that apply in one call."
    ));
    // Preferences stated to one agent must reach every agent, which a
    // client's own memory files never do.
    assert!(guidance.contains(
        "When the user states a preference, decision or rule, or you learn something about a project's code its repository does not state, record it in user-memory (a lesson with scope project), not in your client's own memory files or notes, so every agent and machine sees it."
    ));
    let tools = responses.iter().find(|m| m["id"] == 2).unwrap()["result"]["tools"]
        .as_array()
        .unwrap();
    // Instructions and tool descriptions enter every session's context: keep
    // them within budget (3753 bytes with eight tools, plus 10%; the ninth,
    // read, and the check before work fit by tightening record and update;
    // the tenth, link, by tightening record and the instructions).
    let total = guidance.len()
        + tools
            .iter()
            .map(|t| t["description"].as_str().unwrap_or_default().len())
            .sum::<usize>();
    assert!(
        total <= 4128,
        "instructions and tool descriptions grew to {total} bytes"
    );
    // A category read loads every memory of it, about 1.2k characters each:
    // the description steers an agent to the ids the index shows.
    let description = |tool: &str| {
        tools.iter().find(|t| t["name"] == tool).unwrap()["description"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert!(
        description("read").contains("or a whole category when you need all of it"),
        "{}",
        description("read")
    );
    let mut names: Vec<_> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    names.sort();
    assert_eq!(
        names,
        [
            "export", "forget", "import", "index", "link", "read", "record", "review", "search",
            "update"
        ]
    );
    let property = |tool: &str, field: &str| {
        tools.iter().find(|t| t["name"] == tool).unwrap()["inputSchema"]["properties"][field]
            .clone()
    };
    // What only user-memory's own client sends stays out of the schema every
    // agent loads; the tools still accept it (below).
    for (tool, field) in [
        ("index", "instructions"),
        ("index", "instructions_max_bytes"),
        ("index", "without_global_rules"),
        ("index", "stamps"),
        ("index", "origin_links"),
        ("link", "folder_projects"),
        ("link", "included_projects"),
    ] {
        assert!(property(tool, field).is_null(), "{tool} advertises {field}");
    }
    assert!(property("index", "projects").is_object());
    let update_id = property("update", "memory_id");
    assert_eq!(update_id["type"], "string");
    assert_eq!(update_id["format"], "uuid");
    // Evidence left out keeps the stored evidence.
    assert_eq!(
        property("update", "evidence")["type"],
        serde_json::json!(["string", "null"])
    );
    // Forgetting takes a memory ID or an origin, so the ID is a nullable UUID.
    let forget_id = property("forget", "memory_id");
    assert_eq!(forget_id["type"], serde_json::json!(["string", "null"]));
    assert_eq!(forget_id["format"], "uuid");
    assert_eq!(
        property("forget", "origin")["type"],
        serde_json::json!(["string", "null"])
    );
    assert_eq!(
        property("record", "evidence_ids")["items"]["format"],
        "uuid"
    );
    for list in ["supersedes", "distinct_from"] {
        let schema = property("record", list);
        assert_eq!(schema["type"], "array", "{list}");
        assert_eq!(schema["items"]["format"], "uuid", "{list}");
    }
    let confirms = property("record", "confirms");
    assert_eq!(confirms["type"], serde_json::json!(["string", "null"]));
    assert_eq!(confirms["format"], "uuid");
    assert_eq!(property("record", "merge")["type"], "boolean");
    assert_eq!(property("record", "origin")["type"], "string");
    // Every parameter tells an agent what it is for, on one line.
    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        for (field, schema) in tool["inputSchema"]["properties"].as_object().unwrap() {
            let description = schema["description"].as_str().unwrap_or_default();
            assert!(
                !description.trim().is_empty(),
                "{name}.{field} has no description"
            );
            assert!(
                !description.contains('\n'),
                "{name}.{field}: {description:?}"
            );
        }
    }
    // Agents set it on their own judgment, so it says exactly when it is true.
    assert_eq!(
        property("record", "user_confirmed")["description"],
        "True only when the user, in this conversation, stated or approved this exact memory in their own words and, for scope global, that it holds in every project; for an observation or inference, only once the user approved recording it."
    );
    // The index and the instruction files are built from titles, so the
    // schema says a record needs one. Scope has no default: a global memory
    // applies everywhere, so the agent states which one it means.
    let required = |tool: &str| {
        tools.iter().find(|t| t["name"] == tool).unwrap()["inputSchema"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    for field in ["title", "scope"] {
        assert!(
            required("record").contains(&field.to_owned()),
            "{field}: {:?}",
            required("record")
        );
    }
    assert_eq!(required("link"), ["origin"]);
    // A global memory is the user's call, and a repository a folder always
    // needs is listed for the folder rather than linked again each time.
    for tool in ["record", "update"] {
        assert!(
            property(tool, "scope")["description"]
                .as_str()
                .unwrap()
                .ends_with("global only for what holds in every project, and only with the user's yes (user_confirmed); project for anything about one repository."),
            "{tool}"
        );
    }
    let link = tools.iter().find(|t| t["name"] == "link").unwrap();
    assert!(
        link["description"]
            .as_str()
            .unwrap()
            .contains("use .user-memory @ lines instead")
    );
    assert!(
        property("link", "projects")["description"]
            .as_str()
            .unwrap()
            .contains(
                "(user-memory project-id --include) instead of linking in every conversation"
            )
    );
    // Only these types become rules in the index and the instruction files;
    // a rule recorded under another type would be left out of them.
    for tool in ["record", "update"] {
        let description = property(tool, "type")["description"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            description.contains("preference, constraint or value"),
            "{tool}: {description}"
        );
        assert!(
            description.contains("instruction files"),
            "{tool}: {description}"
        );
    }
}

#[test]
fn tools_apply_documented_defaults_and_require_confirmation_to_change_memory() {
    let tmp = tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_user-memory"))
        .arg("serve")
        .sandboxed(tmp.path())
        .env("USER_MEMORY_DB", tmp.path().join("memory.sqlite3"))
        .env_remove("USER_MEMORY_URL")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    // Read on a thread so a tool that never answers fails the test instead of hanging it.
    let (lines, output) = std::sync::mpsc::channel();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    std::thread::spawn(move || {
        for line in stdout.lines() {
            if lines.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    let mut next_id = 0;
    let mut send = |method: &str, params: Value| -> Value {
        next_id += 1;
        writeln!(
            input,
            "{}",
            json!({"jsonrpc":"2.0","id":next_id,"method":method,"params":params})
        )
        .unwrap();
        if method == "initialize" {
            writeln!(
                input,
                "{}",
                json!({"jsonrpc":"2.0","method":"notifications/initialized"})
            )
            .unwrap();
        }
        input.flush().unwrap();
        loop {
            let line = output
                .recv_timeout(time::STUCK)
                .unwrap_or_else(|err| panic!("No reply to {method}: {err}"));
            let msg: Value = serde_json::from_str(&line).unwrap();
            if msg["id"] == next_id {
                return msg;
            }
        }
    };
    send(
        "initialize",
        json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"integration-test","version":"1"}}),
    );
    let mut tool = |name: &str, arguments: Value| -> String {
        let reply = send("tools/call", json!({"name":name,"arguments":arguments}));
        reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("{name}: {reply}"))
            .to_owned()
    };
    let mut ids = Vec::new();
    for i in 0..9 {
        let saved: Value = serde_json::from_str(&tool(
            "record",
            json!({"type":"fact","category":"workflow","title":format!("Fact {i}"),"content":format!("Fact {i}"),"scope":"global","user_confirmed":true}),
        ))
        .unwrap();
        ids.push(saved["id"].as_str().unwrap().to_owned());
    }
    tool(
        "record",
        json!({"category":"workflow","title":"Untyped note","content":"Untyped note","scope":"global","user_confirmed":true}),
    );
    let read: Value = serde_json::from_str(&tool("read", json!({"ids": [&ids[1][..8]]}))).unwrap();
    assert_eq!(read["memories"][0]["id"], ids[1].as_str(), "{read}");
    let untyped: Value = serde_json::from_str(&tool("search", json!({"topic":"untyped"}))).unwrap();
    assert_eq!(untyped["memories"][0]["type"], "fact");
    // Search returns a page, by default of up to 100 memories.
    let recalled: Value = serde_json::from_str(&tool("search", json!({}))).unwrap();
    assert_eq!(recalled["memories"].as_array().unwrap().len(), 10);
    assert!(recalled.get("next_cursor").is_none());
    // The index lists every memory, whatever the store's size.
    let index = tool("index", json!({}));
    assert_eq!(
        index.lines().filter(|l| l.starts_with("    ")).count(),
        10,
        "{index}"
    );
    let refused: Value = serde_json::from_str(&tool(
        "update",
        json!({"memory_id":ids[0],"content":"Changed"}),
    ))
    .unwrap();
    assert_eq!(refused["updated"], false);
    let updated: Value = serde_json::from_str(&tool(
        "update",
        json!({"memory_id":ids[0],"content":"Changed","user_confirmed":true}),
    ))
    .unwrap();
    assert_eq!(updated["updated"], true);
    let refused: Value =
        serde_json::from_str(&tool("forget", json!({"memory_id":ids[0]}))).unwrap();
    assert_eq!(refused["deleted"], false);
    let deleted: Value = serde_json::from_str(&tool(
        "forget",
        json!({"memory_id":ids[0],"user_confirmed":true}),
    ))
    .unwrap();
    assert_eq!(deleted["deleted"], true);
    // Tools take project keys or remote URLs: the server may be on another
    // machine, where a local path means nothing.
    let refused: Value =
        serde_json::from_str(&tool("search", json!({"projects": ["/srv/notebooks"]}))).unwrap();
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("is a local path"),
        "{refused}"
    );
    let index = tool("index", json!({"projects": ["~/notebooks"]}));
    assert!(index.contains("is a local path"), "{index}");
    let remaining: Value = serde_json::from_str(&tool("search", json!({}))).unwrap();
    assert_eq!(remaining["memories"].as_array().unwrap().len(), 9);
    let mut forget =
        |arguments: Value| -> Value { serde_json::from_str(&tool("forget", arguments)).unwrap() };
    let exactly_one = "Give exactly one of memory_id or a non-empty origin";
    for arguments in [
        json!({"user_confirmed":true}),
        json!({"memory_id":ids[1],"origin":"claude:session/x","user_confirmed":true}),
        json!({"origin":"   ","user_confirmed":true}),
    ] {
        let refused = forget(arguments.clone());
        assert_eq!(refused["deleted"], false, "{arguments}");
        assert_eq!(refused["reason"], exactly_one, "{arguments}");
    }
    assert_eq!(
        forget(json!({"origin":"claude:session/none","user_confirmed":true})),
        json!({"deleted":false,"count":0,"ids":[],"kept":[],"unlinked":0})
    );
    assert_eq!(
        forget(json!({"origin":"claude:session/none"}))["reason"],
        "Forgetting memory requires an explicit user request"
    );
    drop(tool);
    drop(send);
    drop(input);
    child.wait().unwrap();
}

// Links are per conversation and only add: the folder's own project is never
// stored as one and cannot be unlinked, and a local path is refused because
// it would name the project differently on each device.
#[test]
fn link_adds_lists_and_removes_one_conversations_projects() {
    let tmp = tempdir().unwrap();
    let home = tmp.path();
    let notebooks = repository(home, "notebooks", "git@github.com:acme/notebooks.git");
    let call =
        |name: &str, arguments: Value| serve::call(server(home, &notebooks), name, arguments);
    let link = |arguments: Value| call("link", arguments);
    for (title, project) in [
        ("Use tabs in notebooks", "github.com/acme/notebooks"),
        ("Run the fuzzer before merging", "github.com/acme/compiler"),
    ] {
        let stored = call(
            "record",
            json!({"category":"workflow","title":title,"content":title,"scope":"project","project":project,"user_confirmed":true}),
        );
        assert_eq!(stored["stored"], true, "{stored}");
    }
    let needs_origin = "link needs the conversation origin printed at session start; without the session hooks, pass projects to index in each call instead.";
    assert_eq!(
        link(json!({"origin":"  ","projects":["github.com/acme/compiler"]})),
        json!({"linked":false,"reason":needs_origin})
    );
    for path in ["/srv/compiler", "~/compiler", "./compiler", "src\\compiler"] {
        let refused = link(json!({"origin":"claude:session/a","projects":[path]}));
        assert_eq!(refused["linked"], false, "{path}");
        assert!(
            refused["reason"]
                .as_str()
                .unwrap()
                .contains("is a local path"),
            "{path}: {refused}"
        );
    }
    let a = "claude:session/a";
    let linked = link(json!({"origin":a,"projects":["https://github.com/Acme/Compiler.git"]}));
    assert_eq!(linked["linked"], true, "{linked}");
    assert_eq!(linked["added"], json!(["github.com/acme/compiler"]));
    assert_eq!(linked["projects"], json!(["github.com/acme/compiler"]));
    // The reply carries the linked project's index lines, and only those.
    let index = linked["index"].as_str().unwrap();
    assert!(index.contains("Run the fuzzer before merging"), "{index}");
    assert!(!index.contains("Use tabs in notebooks"), "{index}");
    let again = link(json!({"origin":a,"projects":["github.com/acme/compiler"]}));
    assert_eq!(
        again,
        json!({"linked":true,"projects":["github.com/acme/compiler"],"added":[],"notes":["github.com/acme/compiler is already linked"]})
    );
    assert_eq!(
        link(json!({"origin":a,"projects":[]}))["projects"],
        json!(["github.com/acme/compiler"])
    );
    assert_eq!(
        link(json!({"origin":"claude:session/b","projects":[]}))["projects"],
        json!([])
    );
    // The folder's own project always loads here, so linking or unlinking it
    // stores and removes nothing.
    let folder = "github.com/acme/notebooks comes from this folder; it always loads here";
    let own = link(json!({"origin":a,"projects":["git@github.com:acme/notebooks.git"]}));
    assert_eq!(own["added"], json!([]));
    assert_eq!(own["notes"], json!([folder]));
    // Links list in the order they were made; unlinking one keeps the others
    // and every other conversation's.
    let b = "claude:session/b";
    link(json!({"origin":b,"projects":["github.com/acme/compiler"]}));
    let more = link(json!({"origin":a,"projects":["github.com/acme/abc"]}));
    assert_eq!(
        more["projects"],
        json!(["github.com/acme/compiler", "github.com/acme/abc"])
    );
    let unlinked = link(json!({
        "origin":a,
        "projects":["github.com/acme/notebooks","github.com/acme/web","github.com/acme/compiler"],
        "unlink":true
    }));
    assert_eq!(
        unlinked,
        json!({"linked":true,"projects":["github.com/acme/abc"],"removed":["github.com/acme/compiler"],"notes":[folder,"github.com/acme/web is not linked"]})
    );
    assert_eq!(
        link(json!({"origin":b,"projects":[]}))["projects"],
        json!(["github.com/acme/compiler"])
    );
}

// Forgetting a conversation forgets what it linked along with its memories.
#[test]
fn forgetting_a_conversation_removes_its_links() {
    let tmp = tempdir().unwrap();
    let home = tmp.path();
    let call = |name: &str, arguments: Value| serve::call(server(home, home), name, arguments);
    for origin in ["claude:session/a", "claude:session/b"] {
        call(
            "link",
            json!({"origin":origin,"projects":["github.com/acme/compiler"]}),
        );
    }
    let forgot = call(
        "forget",
        json!({"origin":"claude:session/a","user_confirmed":true}),
    );
    assert_eq!(forgot["unlinked"], 1, "{forgot}");
    for (origin, left) in [
        ("claude:session/a", json!([])),
        ("claude:session/b", json!(["github.com/acme/compiler"])),
    ] {
        assert_eq!(
            call("link", json!({"origin":origin,"projects":[]}))["projects"],
            left,
            "{origin}"
        );
    }
}

// A global memory applies in every project, so only the user makes one:
// every source needs their yes, and so does an update that makes one global.
#[test]
fn a_global_memory_needs_the_users_yes_whatever_its_source() {
    let tmp = tempdir().unwrap();
    let home = tmp.path();
    let call = |name: &str, arguments: Value| serve::call(server(home, home), name, arguments);
    let refusal = "A global memory applies in every project: ask the user whether it does and pass user_confirmed, or record it with scope project.";
    let memory = |title: &str, extra: Value| {
        let mut arguments = json!({"category":"workflow","title":title,"content":title});
        arguments
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        arguments
    };
    let mut support = Vec::new();
    for (title, origin) in [
        ("Asks before force-pushing", "claude:session/1"),
        ("Asks before deleting branches", "claude:session/2"),
    ] {
        let stored = call(
            "record",
            memory(
                title,
                json!({"scope":"global","origin":origin,"user_confirmed":true}),
            ),
        );
        assert_eq!(stored["stored"], true, "{stored}");
        support.push(stored["id"].clone());
    }
    for (source, title, scoped_title, extra) in [
        ("explicit", "Prefers squash merges", "", json!({})),
        (
            "observed",
            "Writes commit bodies that explain the reason",
            "Runs the linter before each commit",
            json!({"source":"observed","evidence":"Did so for each commit on 2026-10-17"}),
        ),
        (
            "inferred",
            "Checks with the user before destructive git commands",
            "Confirms history rewrites in this app",
            json!({"source":"inferred","evidence_ids":support}),
        ),
    ] {
        let mut global = memory(title, extra.clone());
        global["scope"] = json!("global");
        assert_eq!(
            call("record", global.clone()),
            json!({"stored":false,"reason":refusal}),
            "{source}"
        );
        // The same memory goes through with the user's yes, or for one project.
        global["user_confirmed"] = json!(true);
        let stored = call("record", global);
        assert_eq!(stored["stored"], true, "{source}: {stored}");
        if source != "explicit" {
            let mut scoped = memory(scoped_title, extra);
            scoped["scope"] = json!("project");
            scoped["project"] = json!("github.com/acme/app");
            let stored = call("record", scoped);
            assert_eq!(stored["stored"], true, "{source}: {stored}");
        }
    }
    let scoped = call(
        "record",
        memory(
            "Run the fuzzer",
            json!({"scope":"project","project":"github.com/acme/app","user_confirmed":true}),
        ),
    );
    assert_eq!(
        call(
            "update",
            json!({"memory_id":scoped["id"],"scope":"global","project":""})
        ),
        json!({"updated":false,"reason":refusal})
    );
    // Scope has no default: the agent says which one it means.
    let unscoped = call(
        "record",
        memory("Prefer Rust", json!({"user_confirmed":true})),
    );
    assert!(
        unscoped.as_str().unwrap().contains("missing field `scope`"),
        "{unscoped}"
    );
}

// A local path names a project differently on each device, so the tools
// refuse one as a memory's project, as index does.
#[test]
fn record_and_update_refuse_a_local_path_as_project() {
    let tmp = tempdir().unwrap();
    let home = tmp.path();
    let call = |name: &str, arguments: Value| serve::call(server(home, home), name, arguments);
    let reason = |path: &str| {
        format!(
            "{path} is a local path; give projects as project keys or git remote URLs (`user-memory project-id` prints the key)"
        )
    };
    let memory = json!({"category":"workflow","title":"Run the tests","content":"Run the tests","scope":"project","user_confirmed":true});
    for path in ["./here", "/srv/app", "~/app"] {
        let mut arguments = memory.clone();
        arguments["project"] = json!(path);
        assert_eq!(
            call("record", arguments),
            json!({"stored":false,"reason":reason(path)}),
            "{path}"
        );
    }
    let mut arguments = memory.clone();
    arguments["project"] = json!("github.com/acme/app");
    let stored = call("record", arguments);
    assert_eq!(
        call(
            "update",
            json!({"memory_id":stored["id"],"project":"../app","user_confirmed":true})
        ),
        json!({"updated":false,"reason":reason("../app")})
    );
}

/// A call that failed reaches the client flagged with `isError`, with the
/// reason as its text; a refusal the agent resolves, a `reason` beside a
/// false success field, is an answer like any other.
#[test]
fn only_a_failed_call_is_flagged_as_an_error() {
    let tmp = tempdir().unwrap();
    let home = tmp.path();
    let result = |name: &str, arguments: Value| serve::result(server(home, home), name, arguments);
    let text = |reply: &Value| reply["content"][0]["text"].as_str().unwrap().to_owned();
    for (name, arguments, said) in [
        (
            "read",
            json!({"ids":["dead"]}),
            "dead is not a memory id: give a full id or its first 8 characters",
        ),
        (
            "search",
            json!({"topic":"tabs","page_size":0}),
            "page_size must be between 1 and 200",
        ),
        (
            "index",
            json!({"projects":["/srv/api"]}),
            "/srv/api is a local path",
        ),
    ] {
        let reply = result(name, arguments);
        assert_eq!(reply["isError"], true, "{name}: {reply}");
        assert!(text(&reply).contains(said), "{name}: {reply}");
    }
    let refused = result(
        "link",
        json!({"origin":"","projects":["github.com/acme/api"]}),
    );
    assert_ne!(refused["isError"], true, "{refused}");
    assert!(text(&refused).contains(r#""linked":false"#), "{refused}");
    let answered = result("index", json!({}));
    assert_ne!(answered["isError"], true, "{answered}");
    assert_eq!(text(&answered), "No user memories recorded yet.");
}

/// The fields user-memory's own client sends, which the tools' schemas leave
/// out, still work: the session hooks ask index for stamps.
#[test]
fn the_clients_own_fields_are_accepted_though_not_advertised() {
    let tmp = tempdir().unwrap();
    let home = tmp.path();
    let call = |name: &str, arguments: Value| serve::call(server(home, home), name, arguments);
    let stored = call(
        "record",
        json!({"category":"workflow","title":"Use tabs","content":"Use tabs","scope":"global","user_confirmed":true}),
    );
    assert_eq!(stored["stored"], true, "{stored}");
    let stamps = call("index", json!({"stamps": true}));
    let stamps = stamps.as_str().unwrap();
    assert!(!stamps.starts_with("USER MEMORY INDEX"), "{stamps}");
    assert!(stamps.contains("Use tabs"), "{stamps}");
}

/// The names agents reach for, `query` and `limit`, work as `topic` and
/// `page_size`; ignored, a search for one topic returned the whole store.
#[test]
fn search_takes_query_and_limit_as_agents_name_them() {
    let tmp = tempdir().unwrap();
    let home = tmp.path();
    let call = |name: &str, arguments: Value| serve::call(server(home, home), name, arguments);
    for title in [
        "Indent with tabs",
        "Tabs in makefiles",
        "Write short replies",
    ] {
        let stored = call(
            "record",
            json!({"category":"workflow","title":title,"content":title,"scope":"global","user_confirmed":true}),
        );
        assert_eq!(stored["stored"], true, "{stored}");
    }
    let found = call("search", json!({"query":"tabs","limit":1}));
    let memories = found["memories"].as_array().unwrap();
    assert_eq!(memories.len(), 1, "{found}");
    assert!(
        memories[0]["title"].as_str().unwrap().contains("abs"),
        "{found}"
    );
    assert!(found.get("next_cursor").is_some(), "{found}");
}

/// index says "Memory unavailable" only when it cannot reach the store; a
/// call it refuses says what to change, so an agent does not take a mistyped
/// call for an outage.
#[test]
fn index_tells_a_refused_call_from_an_unreachable_store() {
    let tmp = tempdir().unwrap();
    let home = tmp.path();
    let refused = serve::result(
        server(home, home),
        "index",
        json!({"projects":["/srv/api"]}),
    );
    let text = refused["content"][0]["text"].as_str().unwrap();
    assert_eq!(refused["isError"], true, "{refused}");
    assert!(text.starts_with("/srv/api is a local path"), "{text}");
    // A store that is no database.
    std::fs::write(home.join("memory.sqlite3"), "not a database").unwrap();
    let broken = serve::result(server(home, home), "index", json!({}));
    let text = broken["content"][0]["text"].as_str().unwrap();
    assert_eq!(broken["isError"], true, "{broken}");
    assert!(text.starts_with("Memory unavailable: "), "{text}");
}
