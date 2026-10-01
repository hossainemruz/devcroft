//! Provider-neutral handoff to an ordinary interactive Agent session.
use super::session::{Bundle, Capture, Store, digest, pr};
use anyhow::{Context as _, Result, ensure};
use std::{
    collections::BTreeMap,
    fs,
    io::Write as _,
    path::{Component, Path, PathBuf},
};

#[derive(Clone)]
pub(super) struct Workspace {
    pub directory: PathBuf,
    pub checkout: PathBuf,
    pub checkout_head: Option<String>,
    pub capture: String,
    captured: std::sync::Arc<Capture>,
    pub instructions: String,
}
impl Workspace {
    pub fn prepare(
        store: &Store,
        capture: &Capture,
        bundle: Option<&Bundle>,
        cwd: &Path,
        focus: &str,
    ) -> Result<Self> {
        let checkout = if capture.pr.is_some() {
            pr::authoring_checkout(capture)?
        } else {
            cwd.canonicalize()?
        };
        let directory = store.authoring_directory(capture);
        fs::create_dir_all(&directory)?;
        ensure!(
            !fs::symlink_metadata(&directory)?.file_type().is_symlink(),
            "Authoring workspace cannot be a symlink"
        );
        write(
            &directory,
            "capture.json",
            &serde_json::to_string_pretty(capture)?,
        )?;
        write(&directory, "orientation.md", &capture.prompt())?;
        write(
            &directory,
            "runtime.md",
            include_str!("../../assets/review-runtime/README.md"),
        )?;
        write(&directory, "commit.py", COMMIT_SCRIPT)?;
        write(&directory, "request.md", focus)?;
        write(
            &directory,
            "viewed-guide.json",
            &serde_json::to_string_pretty(&bundle)?,
        )?;
        let instructions = format!(
            "# Create and maintain a guided code review\n\n\
             You are running in the ordinary native Agent pane with the user's normal tools, settings and permissions.\n\
             Working checkout: {}\nCaptured revision: {}\nGuide directory: {}\n\n\
             Read request.md for the current reviewer instructions and runtime.md for rendering/SDK constraints.\n\
             Read capture.json for the immutable review source, complete changed-file inventory and registered evidence IDs.\n\
             Explore the checkout, related repositories, documentation and tools as needed under your normal permissions.\n\
             The live checkout may differ from this capture: ground claims in capture.json and clearly label extra context.\n\
             Create or update manifest.json and chapter HTML body fragments in this guide directory.\n\
             Existing files are editable continuation context. Keep stable chapter and claim IDs when their meaning is unchanged.\n\
             Read viewed-guide.json for the currently displayed validated guide; it can differ from unfinished files or after restoring history. Respect that selected guide and do not silently discard unfinished edits.\n\
             Build a useful behavior-oriented review with rich layouts, inline CSS/SVG/JavaScript and deliberate interactive diagrams when they clarify the change.\n\
             Include concise before/after reasoning, consequences, assumptions, a complete text summary, evidence-linked claims and reviewer questions.\n\
             Do not fabricate evidence or test execution. Do not change source or publish a review unless the reviewer asks.\n\n\
             Manifest schema (all fields required; no additional fields):\n\
             {{\"runtime\":1,\"capture\":\"{}\",\"title\":\"…\",\"summary\":\"…\",\"chapters\":[\n\
             {{\"id\":\"behavior-id\",\"title\":\"…\",\"summary\":\"readable explanation\",\"document\":\"chapters/behavior.html\",\n\
             \"evidence_ids\":[\"REGISTERED_ID\"],\"claims\":[{{\"id\":\"claim-id\",\"text\":\"…\",\"evidence_ids\":[\"REGISTERED_ID\"]}}],\"questions\":[\"…\"]}}]}}\n\n\
             Use 1–16 chapters, 1–32 evidence references and 1–24 claims per chapter.\n\
             IDs are unique across chapters/claims, at most 80 ASCII letters/digits/hyphens/underscores.\n\
             Each chapter has its own relative document path; no symlinks or parent-directory traversal.\n\
             Titles ≤240 bytes; guide summary ≤4000 bytes; chapter summary ≤8000 bytes; claims/questions ≤2000 bytes.\n\
             Chapter documents ≤512 KiB each, ≤4 MiB combined. Every claim cites evidence from its chapter.\n\n\
             After every complete revision run: python3 {}\n\
             This writes ready.json last with SHA-256 hashes of the manifest and chapter files.\n\
             The app validates and renders committed updates automatically; partial writes preserve the last guide.\n\
             Keep chatting here: answer questions, investigate source, and update/commit the guide when asked.\n",
            checkout.display(),
            capture.id,
            directory.display(),
            capture.id,
            crate::agent_activity::shell_quote(&directory.join("commit.py").to_string_lossy()),
        );
        write(&directory, "instructions.md", &instructions)?;
        if !directory.join("manifest.json").exists()
            && let Some(bundle) = bundle
        {
            bundle.validate(capture)?;
            let manifest = serde_json::to_string_pretty(&bundle.manifest)?;
            write(&directory, "manifest.json", &manifest)?;
            let mut hashes = BTreeMap::from([("manifest.json".to_owned(), digest(&manifest))]);
            for (path, html) in &bundle.documents {
                write(&directory, path, html)?;
                hashes.insert(path.clone(), digest(html));
            }
            write(
                &directory,
                "ready.json",
                &serde_json::to_string(&serde_json::json!({"capture":capture.id,"files":hashes}))?,
            )?;
        }
        Ok(Self {
            directory,
            checkout_head: pr::checkout_head(&checkout),
            checkout,
            capture: capture.id.clone(),
            captured: std::sync::Arc::new(capture.clone()),
            instructions,
        })
    }
    pub fn prompt(&self) -> String {
        format!(
            "Create or update my guided review. Read and follow the authoring instructions at {}. Read request.md beside it for my focus and context. Keep this session open for follow-up discussion and guide edits.",
            self.directory.join("instructions.md").display()
        )
    }
    pub fn update_viewed_guide(&self, capture: &str, bundle: Option<&Bundle>) -> Result<()> {
        ensure!(
            self.capture == capture,
            "Viewed guide belongs to another capture"
        );
        if let Some(bundle) = bundle {
            bundle.validate(&self.captured)?;
        }
        write(
            &self.directory,
            "viewed-guide.json",
            &serde_json::to_string_pretty(&bundle)?,
        )
    }
    pub fn load(&self, capture: &str) -> Result<Option<Bundle>> {
        ensure!(
            self.capture == capture,
            "Authoring session belongs to another capture"
        );
        if !self.directory.join("ready.json").try_exists()? {
            return Ok(None);
        }
        Bundle::load_committed(&self.directory, &self.captured).map(Some)
    }
}

fn write(directory: &Path, name: &str, text: &str) -> Result<()> {
    ensure!(
        !fs::symlink_metadata(directory)?.file_type().is_symlink(),
        "Authoring workspace cannot be a symlink"
    );
    let path = Path::new(name);
    ensure!(
        !path.is_absolute() && path.components().all(|c| matches!(c, Component::Normal(_))),
        "Authoring path escapes its workspace"
    );
    let destination = directory.join(path);
    let parent = destination.parent().context("Missing authoring parent")?;
    let mut current = directory.to_path_buf();
    for component in path.parent().into_iter().flat_map(Path::components) {
        current.push(component);
        fs::create_dir_all(&current)?;
        ensure!(
            !fs::symlink_metadata(&current)?.file_type().is_symlink(),
            "Authoring directories cannot be symlinks"
        );
    }
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(text.as_bytes())?;
    temp.persist(&destination).map_err(|e| e.error)?;
    Ok(())
}

const COMMIT_SCRIPT: &str = r#"# Commit a finished guide revision; the app still validates every field.
import hashlib, json, os, pathlib, tempfile
root = pathlib.Path(__file__).resolve().parent
manifest = json.loads((root / 'manifest.json').read_text())
files = {}
for name in ['manifest.json'] + [c['document'] for c in manifest['chapters']]:
    path = pathlib.Path(name)
    if path.is_absolute() or '..' in path.parts or (root / path).resolve().is_relative_to(root) is False:
        raise ValueError('Guide path escapes its workspace')
    files[name] = hashlib.sha256((root / path).read_bytes()).hexdigest()
payload = {'capture': manifest['capture'], 'files': files}
with tempfile.NamedTemporaryFile(mode='w', dir=root, delete=False) as output:
    json.dump(payload, output)
    output.flush()
    os.fsync(output.fileno())
    temp = output.name
os.replace(temp, root / 'ready.json')
print('Guide revision committed. Check the native authoring status for validation.')
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::session::{File, Review};

    fn capture(source: &str) -> Capture {
        Capture::from_files(
            "Synthetic review",
            "base",
            "head",
            None,
            vec![File {
                path: "retry.rs".into(),
                old_path: None,
                status: "modified".into(),
                additions: 1,
                deletions: 1,
                old: Some("send();\n".into()),
                new: Some(source.into()),
                lines: vec![],
                unavailable: None,
                truncated: false,
            }],
        )
        .unwrap()
    }
    fn bundle(capture: &Capture) -> Bundle {
        serde_json::from_value(serde_json::json!({
            "manifest": {"runtime":1,"capture":capture.id,"title":"Retry identity","summary":"Inspect retry behavior","chapters":[{
                "id":"retry","title":"Retry preserves identity","summary":"A readable source-linked explanation",
                "document":"chapters/retry.html","evidence_ids":[capture.evidence[0].id],
                "claims":[{"id":"retry-claim","text":"Inspect the captured call","evidence_ids":[capture.evidence[0].id]}],
                "questions":["Does a retry preserve identity?"]
            }]}, "documents":{"chapters/retry.html":"<p>Original guide</p>"}
        })).unwrap()
    }
    fn commit(workspace: &Workspace) {
        let output = std::process::Command::new("python3")
            .arg(workspace.directory.join("commit.py"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "Commit helper failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    #[test]
    fn chat_edits_commit_atomically_and_keep_history_and_invalid_updates_out() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::at(tmp.path().join("saved"));
        let capture = capture("send_with_identity();\n");
        let original = bundle(&capture);
        let workspace = Workspace::prepare(
            &store,
            &capture,
            Some(&original),
            tmp.path(),
            "Explain retry identity",
        )
        .unwrap();
        let mut review = Review::start(capture.clone());
        review
            .install(workspace.load(&capture.id).unwrap().unwrap())
            .unwrap();
        assert_eq!(workspace.checkout, tmp.path().canonicalize().unwrap());
        assert!(
            fs::read_to_string(workspace.directory.join("request.md"))
                .unwrap()
                .contains("retry identity")
        );
        let html = workspace.directory.join("chapters/retry.html");
        fs::write(&html, "<svg>Chat edit</svg>").unwrap();
        assert!(workspace.load(&capture.id).is_err()); // old marker cannot authorize changed bytes
        assert_eq!(
            review.active().bundle.as_ref().unwrap().documents,
            original.documents
        );
        commit(&workspace);
        review
            .install(workspace.load(&capture.id).unwrap().unwrap())
            .unwrap();
        assert_eq!(review.active().guide_history.len(), 1);
        assert!(
            review.active().bundle.as_ref().unwrap().documents["chapters/retry.html"]
                .contains("Chat edit")
        );
        let manifest = workspace.directory.join("manifest.json");
        workspace
            .update_viewed_guide(&capture.id, review.active().bundle.as_ref())
            .unwrap();
        let selected = || {
            serde_json::from_slice::<Option<Bundle>>(
                &fs::read(workspace.directory.join("viewed-guide.json")).unwrap(),
            )
            .unwrap()
        };
        assert_eq!(selected().as_ref(), review.active().bundle.as_ref());
        let old = digest(serde_json::to_vec(&review.active().guide_history[0]).unwrap());
        review.select_guide(&old).unwrap();
        workspace
            .update_viewed_guide(&capture.id, review.active().bundle.as_ref())
            .unwrap();
        assert_eq!(selected(), Some(original.clone()));
        assert!(fs::read_to_string(&html).unwrap().contains("Chat edit"));
        assert!(
            workspace
                .update_viewed_guide("another-capture", None)
                .is_err()
        );
        assert_eq!(selected(), Some(original.clone()));
        let mut data: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
        data["chapters"][0]["evidence_ids"] = serde_json::json!(["invented-source"]);
        fs::write(&manifest, serde_json::to_vec(&data).unwrap()).unwrap();
        commit(&workspace);
        assert!(workspace.load(&capture.id).is_err());
        assert!(
            workspace
                .load(&self::capture("another_revision();\n").id)
                .is_err()
        );
        assert!(
            review.active().bundle.as_ref().unwrap().documents["chapters/retry.html"]
                .contains("Original guide")
        );
    }
    #[test]
    fn replacing_an_agent_preserves_unfinished_editable_files_and_capture_contract() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::at(tmp.path().join("saved"));
        let capture = capture("send();\n");
        let guide = bundle(&capture);
        let workspace =
            Workspace::prepare(&store, &capture, Some(&guide), tmp.path(), "first").unwrap();
        fs::write(
            workspace.directory.join("chapters/retry.html"),
            "Unfinished follow-up edit",
        )
        .unwrap();
        let reopened =
            Workspace::prepare(&store, &capture, Some(&guide), tmp.path(), "second").unwrap();
        assert_eq!(
            fs::read_to_string(reopened.directory.join("chapters/retry.html")).unwrap(),
            "Unfinished follow-up edit"
        );
        assert_eq!(
            fs::read_to_string(reopened.directory.join("request.md")).unwrap(),
            "second"
        );
        assert_eq!(
            serde_json::from_slice::<Capture>(
                &fs::read(reopened.directory.join("capture.json")).unwrap()
            )
            .unwrap()
            .id,
            capture.id
        );
        assert!(reopened.load(&capture.id).is_err());
    }
    #[test]
    fn author_commit_cannot_use_symlinks_or_missing_or_extra_files() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::at(tmp.path().join("saved"));
        let capture = capture("send();\n");
        let workspace =
            Workspace::prepare(&store, &capture, Some(&bundle(&capture)), tmp.path(), "").unwrap();
        let ready = workspace.directory.join("ready.json");
        let mut marker: serde_json::Value =
            serde_json::from_slice(&fs::read(&ready).unwrap()).unwrap();
        marker["files"]["extra.html"] = serde_json::json!(digest("extra"));
        fs::write(&ready, serde_json::to_vec(&marker).unwrap()).unwrap();
        assert!(workspace.load(&capture.id).is_err());
        commit(&workspace);
        #[cfg(unix)]
        {
            let html = workspace.directory.join("chapters/retry.html");
            fs::rename(&html, tmp.path().join("outside.html")).unwrap();
            std::os::unix::fs::symlink(tmp.path().join("outside.html"), &html).unwrap();
            assert!(workspace.load(&capture.id).is_err());
        }
    }
}
