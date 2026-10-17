//! Local-host REPL control and actual provider-request injection; never model-paid calls.
use kroma_core::budget::BudgetConfig;
use kroma_core::messages::{AssistantTurn,FinishReason,TranscriptItem};
use kroma_core::provider::FakeProvider;
use kroma_core::revision::SnapshotStore;
use kroma_core::surface::{InteractiveRepl,KromaRuntimeConfig,RpcDispatcher,JsonRpcRequest};
use kroma_core::tool::policy::PolicyMode;
use kroma_core::workspace::WorkspaceCap;
use std::path::PathBuf;
use std::sync::Arc;

struct Tree(PathBuf);
impl Tree {
    fn new()->Self {let p=std::env::temp_dir().join(format!("exp-{}",uuid::Uuid::new_v4()));std::fs::create_dir_all(&p).unwrap();Self(p)}
}
impl Drop for Tree {fn drop(&mut self){let _=std::fs::remove_dir_all(&self.0);}}
fn stop()->AssistantTurn {AssistantTurn{content:Some("ok".into()),reasoning_blocks:vec![],tool_calls:vec![],finish_reason:FinishReason::Stop,usage:None}}
fn manifest(root:&Tree, ws:&std::path::Path, revision:&str)->PathBuf {
    let private=root.0.join("operator");std::fs::create_dir_all(&private).unwrap();
    #[cfg(unix)] {use std::os::unix::fs::PermissionsExt;std::fs::set_permissions(&private,std::fs::Permissions::from_mode(0o700)).unwrap();}
    let file=private.join("candidates.json");
    std::fs::write(&file,serde_json::to_vec(&serde_json::json!({
        "workspace_root":ws,"revision":revision,"records":[{
            "key":"reviewed","premise":"touching the parser","action":"EXPERIENCE_REVIEWED_SENTINEL: check parser boundaries",
            "evidence":"operator inspected local deterministic run","required_policy":"allow-write",
            "conditions":[]
        }]})).unwrap()).unwrap();
    #[cfg(unix)] {use std::os::unix::fs::PermissionsExt;std::fs::set_permissions(&file,std::fs::Permissions::from_mode(0o600)).unwrap();}
    file
}
fn has_experience(request:&kroma_core::provider::ProviderRequest)->bool {
    request.messages.iter().any(|item|matches!(item,TranscriptItem::User(user) if user.content.contains("EXPERIENCE_REVIEWED_SENTINEL")))
}

#[tokio::test]
async fn real_repl_accept_enable_revoke_and_off_control_ephemeral_provider_context() {
    let root=Tree::new();let ws=root.0.join("ws");std::fs::create_dir_all(&ws).unwrap();std::fs::write(ws.join("lib.txt"),"v1").unwrap();
    let cap=WorkspaceCap::new(&ws).unwrap();let state=root.0.join("state");
    let store=SnapshotStore::new(state.join("snapshots"));let rev=store.capture_revision(&cap,None,None,0).unwrap();
    let file=manifest(&root,&ws,rev.revision_id.as_str());
    let provider=FakeProvider::new(vec![stop();6]);let observed=provider.clone();
    let mut cfg=KromaRuntimeConfig::new(&ws,&state,PolicyMode::AllowWrite);cfg.stream=false;cfg.quiet=true;
    cfg.budget=BudgetConfig::default();
    let dispatcher=RpcDispatcher::new(cfg,Arc::new(provider)).unwrap();let repl=InteractiveRepl::new(dispatcher.clone());
    assert!(repl.handle_line("/experience status").await.unwrap().output.contains("experience: off"));
    let load=repl.handle_line(&format!("/experience load {}",file.display())).await.unwrap();
    assert!(load.output.contains("all unaccepted"),"{load:?}");
    repl.handle_line("/experience on").await.unwrap();repl.handle_line("first").await.unwrap();
    assert!(!has_experience(&observed.recorded_requests()[0]),"loading/enabling cannot mint acceptance");
    assert!(repl.handle_line("/experience accept repo:reviewed").await.unwrap().output.contains("accepted repo:reviewed"));
    repl.handle_line("second").await.unwrap();assert!(has_experience(&observed.recorded_requests()[1]));
    repl.handle_line("/experience off").await.unwrap();repl.handle_line("third").await.unwrap();
    assert!(!has_experience(&observed.recorded_requests()[2]),"off removes injection even on resumed transcript");
    repl.handle_line("/experience on").await.unwrap();repl.handle_line("/experience revoke repo:reviewed").await.unwrap();
    repl.handle_line("fourth").await.unwrap();assert!(!has_experience(&observed.recorded_requests()[3]));
    // The local-only API is deliberately not a remotely callable JSON method.
    assert!(dispatcher.handle_request(JsonRpcRequest::new(9,"experience.accept",serde_json::json!({"id":"repo:reviewed"}))).await.error.is_some());
    let requests=observed.recorded_requests();
    for request in &requests[1..] {assert_eq!(request.system_prompt,requests[0].system_prompt);assert_eq!(request.tools,requests[0].tools);}
    let injected=requests[1].messages.iter().filter_map(|item|match item {TranscriptItem::User(user) if user.content.contains("EXPERIENCE_REVIEWED_SENTINEL")=>Some(user.content.len()),_=>None}).sum::<usize>();
    assert!(injected<=2048,"dynamic experience must stay within the declared byte budget");
}

#[tokio::test]
async fn stale_revision_and_untrusted_workspace_imports_are_refused() {
    let root=Tree::new();let ws=root.0.join("ws");std::fs::create_dir_all(&ws).unwrap();std::fs::write(ws.join("lib.txt"),"v1").unwrap();
    let cap=WorkspaceCap::new(&ws).unwrap();let state=root.0.join("state");let store=SnapshotStore::new(state.join("snapshots"));
    let revision=store.capture_revision(&cap,None,None,0).unwrap();let file=manifest(&root,&ws,revision.revision_id.as_str());
    let provider=FakeProvider::new(vec![stop();2]);let observed=provider.clone();
    let mut cfg=KromaRuntimeConfig::new(&ws,&state,PolicyMode::AllowWrite);cfg.quiet=true;cfg.stream=false;
    let repl=InteractiveRepl::new(RpcDispatcher::new(cfg,Arc::new(provider)).unwrap());
    repl.handle_line(&format!("/experience load {}",file.display())).await.unwrap();
    repl.handle_line("/experience accept repo:reviewed").await.unwrap();repl.handle_line("/experience on").await.unwrap();
    std::fs::write(ws.join("lib.txt"),"v2").unwrap();repl.handle_line("now on changed code").await.unwrap();
    assert!(!has_experience(&observed.recorded_requests()[0]),"old revision is never assumed applicable");
    let inside=ws.join("evil.json");std::fs::copy(file,&inside).unwrap();
    let result=repl.handle_line(&format!("/experience load {}",inside.display())).await.unwrap();
    assert!(result.output.contains("outside the agent workspace"),"{}",result.output);
}

#[test]
fn imported_data_cannot_self_accept_and_policy_conditions_expiry_are_enforced() {
    use kroma_core::experience_runtime::ExperienceSession;
    let root=Tree::new();let ws=root.0.join("ws");std::fs::create_dir_all(&ws).unwrap();std::fs::write(ws.join("lib.txt"),"v1").unwrap();
    let cap=WorkspaceCap::new(&ws).unwrap();let store=SnapshotStore::new(root.0.join("state/snapshots"));
    let rev=store.capture_revision(&cap,None,None,0).unwrap();let file=manifest(&root,&ws,rev.revision_id.as_str());
    let original=std::fs::read(&file).unwrap();
    let mut spoof:serde_json::Value=serde_json::from_slice(&original).unwrap();
    spoof["records"][0]["status"]=serde_json::json!("accepted");
    std::fs::write(&file,serde_json::to_vec(&spoof).unwrap()).unwrap();
    assert!(ExperienceSession::default().load(&file,&cap,&rev.revision_id).is_err(),"JSON cannot mint acceptance");
    std::fs::write(&file,&original).unwrap();
    let mut session=ExperienceSession::default();session.load(&file,&cap,&rev.revision_id).unwrap();
    assert!(session.control("accept",Some("repo:reviewed"),&cap,&rev.revision_id,PolicyMode::ReadOnly).is_err());
    session.control("accept",Some("repo:reviewed"),&cap,&rev.revision_id,PolicyMode::AllowWrite).unwrap();
    session.control("on",None,&cap,&rev.revision_id,PolicyMode::AllowWrite).unwrap();
    assert!(session.injection(&cap,Some(&rev.revision_id),PolicyMode::ReadOnly).is_empty(),"experience never grants another policy");
    assert!(!session.injection(&cap,Some(&rev.revision_id),PolicyMode::AllowWrite).is_empty());
    session.control("revoke",Some("repo:reviewed"),&cap,&rev.revision_id,PolicyMode::AllowWrite).unwrap();
    assert!(session.load(&file,&cap,&rev.revision_id).is_err(),"reload cannot clear a local revocation");
    for expired in [false,true] {
        let mut bad:serde_json::Value=serde_json::from_slice(&original).unwrap();
        if expired {bad["records"][0]["expires_at_ms"]=serde_json::json!(1);}
        else {bad["records"][0]["conditions"]=serde_json::json!([{"path":"lib.txt","sha256":"0".repeat(64)}]);}
        std::fs::write(&file,serde_json::to_vec(&bad).unwrap()).unwrap();
        let mut session=ExperienceSession::default();session.load(&file,&cap,&rev.revision_id).unwrap();
        assert!(session.control("accept",Some("repo:reviewed"),&cap,&rev.revision_id,PolicyMode::AllowWrite).is_err());
    }
}

#[test]
fn many_reviewed_records_are_bounded_and_public_or_symlink_imports_fail_closed() {
    use kroma_core::experience_runtime::ExperienceSession;
    let root=Tree::new();let ws=root.0.join("ws");std::fs::create_dir_all(&ws).unwrap();
    let cap=WorkspaceCap::new(&ws).unwrap();let store=SnapshotStore::new(root.0.join("state/snapshots"));
    let rev=store.capture_revision(&cap,None,None,0).unwrap();let file=manifest(&root,&ws,rev.revision_id.as_str());
    let mut data:serde_json::Value=serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();let mut records=vec![];
    for index in 0..20 {let mut r=data["records"][0].clone();r["key"]=serde_json::json!(format!("r{index}"));r["action"]=serde_json::json!("x".repeat(250));records.push(r);}
    data["records"]=serde_json::json!(records);std::fs::write(&file,serde_json::to_vec(&data).unwrap()).unwrap();
    let mut session=ExperienceSession::default();session.load(&file,&cap,&rev.revision_id).unwrap();
    for index in 0..20 {session.control("accept",Some(&format!("repo:r{index}")),&cap,&rev.revision_id,PolicyMode::AllowWrite).unwrap();}
    session.control("on",None,&cap,&rev.revision_id,PolicyMode::AllowWrite).unwrap();
    let block=session.injection(&cap,Some(&rev.revision_id),PolicyMode::AllowWrite);assert!(!block.is_empty());assert!(block.len()<=2048);
    #[cfg(unix)] {
        use std::os::unix::fs::{PermissionsExt,symlink};
        let link=file.with_file_name("link.json");symlink(&file,&link).unwrap();
        assert!(ExperienceSession::default().load(&link,&cap,&rev.revision_id).is_err());
        std::fs::set_permissions(&file,std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(ExperienceSession::default().load(&file,&cap,&rev.revision_id).is_err());
    }
}
