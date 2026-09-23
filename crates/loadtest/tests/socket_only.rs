//! The load tool must reach the server over the socket only. Linking
//! `rustibia-server` for its protocol types also exposes its game logic, and a
//! call straight into the server's game or actor modules would bypass
//! everything this tool exists to measure.

use std::fs;
use std::path::Path;

#[test]
fn the_bot_reaches_the_server_only_over_the_socket() {
    let mut offenders = Vec::new();
    visit(Path::new("src"), &mut offenders);

    assert!(
        offenders.is_empty(),
        "these files reach into the server's game logic instead of the socket: {offenders:?}"
    );
}

/// Matches the bare path segments rather than the full `rustibia_server::` path,
/// because a grouped import never spells that path out. Comments are stripped
/// first: naming the server function a two-sided agreement mirrors is what earns
/// a comment here.
fn visit(dir: &Path, offenders: &mut Vec<String>) {
    for entry in fs::read_dir(dir).expect("reading the source tree") {
        let path = entry.expect("a directory entry").path();

        if path.is_dir() {
            visit(&path, offenders);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let source = fs::read_to_string(&path).expect("reading a source file");
            let code: String = source
                .lines()
                .map(|line| line.split("//").next().unwrap_or(""))
                .collect::<Vec<_>>()
                .join("\n");

            if code.contains("game::") || code.contains("actors::") {
                offenders.push(path.display().to_string());
            }
        }
    }
}
