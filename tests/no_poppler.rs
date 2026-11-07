//! `read_file` on a PDF when poppler's tools are not on the `PATH`. One test,
//! in a process of its own, because it empties the `PATH`.

mod support;

use just_harness::agent::{Agent, AllowAll};
use just_harness::api::Client;
use just_harness::tools::{ToolContext, builtin};
use serde_json::{Value, json};
use support::{FakeTransport, config, sent_body, text_reply, tool_reply, user};

async fn read(root: &std::path::Path, input: Value) -> Value {
    let transport = FakeTransport::new()
        .respond(
            200,
            vec![&tool_reply(&[("t1", "read_file", input)], "tool_use")],
        )
        .respond(200, vec![&text_reply("ok", "end_turn")]);
    let agent = Agent::new(
        Client::with_transport(config(), &transport),
        "claude-haiku-5-5",
        builtin(),
        ToolContext::new(root).unwrap(),
    );
    agent
        .run_turn(&mut vec![user("go")], &mut (), &mut AllowAll)
        .await
        .unwrap();
    sent_body(&transport, 1)["messages"][2]["content"][0].clone()
}

#[tokio::test]
async fn a_pdf_is_sent_whole_and_a_page_range_says_what_to_install() {
    // SAFETY: this is the only test in this process, and nothing else here
    // reads the environment while it is set.
    unsafe { std::env::set_var("PATH", "") };
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.pdf"), "%PDF-1.4\n").unwrap();

    let whole = read(dir.path(), json!({ "path": "a.pdf" })).await;
    assert_eq!(whole["content"][1]["type"], "document");

    let range = read(dir.path(), json!({ "path": "a.pdf", "pages": "1-2" })).await;
    assert_eq!(range["is_error"], true);
    assert_eq!(
        range["content"],
        "pdftoppm is not installed. Install poppler-utils (e.g. `brew install poppler` or `apt-get install poppler-utils`) to enable PDF page rendering."
    );
}
