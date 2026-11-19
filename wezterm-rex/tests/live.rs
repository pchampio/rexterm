use anyhow::{Context, Result, ensure};
use ghostty_vt::fmt::{Format, Formatter, FormatterOptions};
use serde_json::json;
use std::time::Duration;
use wezterm_rex::{
    snapshot::SnapshotBuffer,
    transport::{self, Client},
    wire,
};

// Only run on an explicitly supplied disposable server. Never touches the user's server.
#[tokio::test]
#[ignore = "requires REX_TEST_ENDPOINT pointing to an isolated server"]
async fn snapshot_and_resume_against_rex() -> Result<()> {
    let endpoint = std::env::var("REX_TEST_ENDPOINT").context("explicit test server required")?;
    let (client, _events) = Client::connect(endpoint.clone()).await?;
    ensure!(
        client
            .call("wezterm.nonexistent_method", json!({}))
            .await
            .is_err(),
        "API error was swallowed"
    );
    let replies = futures_util::future::join_all(
        (0..32).map(|_| client.call_reply("server.version", json!({}))),
    )
    .await;
    for reply in replies {
        let reply = reply?;
        ensure!(reply.payload.is_object(), "concurrent response misrouted");
    }

    let created=client.call("session.create",json!({"label":"wezterm-rex-protocol-test","initial_windows":[{"layout":{"block":{"flavor":"com.superlogical.terminal.shell","options":{"shell":"none","login":false,"command":["/bin/sh"]}}}}]})).await?;
    let session = created["session_id"].as_str().context("session id")?;
    let block = created["initial_windows"][0]["block_ids"][0]
        .as_str()
        .context("block id")?;
    let result=async {
        client.call("session.attach",json!({"session_id":session})).await?;
        client.call("com.superlogical.terminal.resize",json!({"session_id":session,"block_id":block,"args":{"columns":100,"rows":24}})).await?;
        client.call("com.superlogical.terminal.write",json!({"session_id":session,"block_id":block,"args":{"data":"i=1; while [ $i -le 160 ]; do printf 'REXSEQ_%04d\\n' $i; i=$((i+1)); sleep 0.01; done; printf 'REX_UNICODE_界_café_é\\nREX_DONE\\n'\n"}})).await?;
        tokio::time::sleep(Duration::from_millis(120)).await;
        let ticket=client.call("data.connect",json!({"session_id":session,"block_id":block,"data_ref_id":"terminal","subprotocol":wire::PROTOCOL})).await?;
        let mut ws=transport::socket(&endpoint,"/api/data",wire::PROTOCOL).await?;
        let welcome=wire::handshake(&mut ws,ticket["ticket"].as_str().context("ticket")?).await.context("initial data handshake")?;
        let mut buffer=SnapshotBuffer::default(); let mut term=None; let mut received=0;
        let mut parser=wire::ApcParser::new(welcome.nonce);
        while term.is_none() {
            let bytes=tokio::time::timeout(Duration::from_secs(10),wire::next_binary(&mut ws)).await??;
            received+=bytes.len() as u64;
            for chunk in bytes.chunks(17) {
                if let Some(t)=term.as_mut() { feed(t,&mut parser,chunk)?; }
                else if let Some((mut t,tail))=buffer.push(chunk)? {feed(&mut t,&mut parser,&tail)?;term=Some(t);}
            }
        }
        ws.close(None).await?;
        drop(ws);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut ws=transport::socket(&endpoint,"/api/data",wire::PROTOCOL).await?;
        let resumed=wire::resume(&mut ws,&welcome.token,received).await.context("resumed data handshake")?;
        ensure!(resumed.nonce==welcome.nonce,"resume changed nonce");
        ensure!(resumed.token!=welcome.token,"resume failed to rotate token");
        let mut term=term.unwrap();
        loop {
            let text=text(&term)?;
            if text.lines().any(|l|l.trim()=="REX_DONE") {
                let nums:Vec<u32>=text.lines().filter_map(|l|l.trim().strip_prefix("REXSEQ_")?.parse().ok()).collect();
                ensure!(nums==(1..=160).collect::<Vec<_>>(),"gaps or duplicates: {} lines",nums.len());
                ensure!(text.contains("REX_UNICODE_界_café_é"),"Unicode lost");break;
            }
            let bytes=tokio::time::timeout(Duration::from_secs(10),wire::next_binary(&mut ws)).await??;
            for chunk in bytes.chunks(7) {feed(&mut term,&mut parser,chunk)?;}
        }
        Ok::<_,anyhow::Error>(())
    }.await;
    let cleanup = client
        .call("session.destroy", json!({"session_id":session}))
        .await;
    cleanup?;
    result
}
fn feed(t: &mut ghostty_vt::Terminal, p: &mut wire::ApcParser, b: &[u8]) -> Result<()> {
    for part in p.feed(b)? {
        match part {
            wire::Part::Output(b) => t.vt_write(&b),
            wire::Part::Metadata(v) => {
                if v["type"] == "resize" {
                    t.resize(
                        v["cols"].as_u64().context("cols")? as u16,
                        v["rows"].as_u64().context("rows")? as u16,
                        0,
                        0,
                    )?;
                }
            }
        }
    }
    Ok(())
}
fn text(t: &ghostty_vt::Terminal) -> Result<String> {
    Ok(String::from_utf8(
        Formatter::new(
            t,
            FormatterOptions::new()
                .with_format(Format::Plain)
                .with_unwrap(true),
        )?
        .format_alloc()?
        .to_vec(),
    )?)
}

#[tokio::test]
#[ignore = "requires REX_TEST_ENDPOINT pointing to an isolated server"]
async fn native_actor_cells_history_and_resize() -> Result<()> {
    native_actor_cells_history_and_resize_impl(true).await
}
#[tokio::test]
#[ignore = "requires REX_TEST_ENDPOINT pointing to an isolated server"]
async fn native_actor_early_input_during_snapshot_setup() -> Result<()> {
    native_actor_cells_history_and_resize_impl(false).await
}
async fn native_actor_cells_history_and_resize_impl(wait_ready:bool) -> Result<()> {
    use wezterm_rex::actor::Actor;
    #[derive(Debug)] struct TestConfig;
    impl wezterm_term::TerminalConfiguration for TestConfig {fn color_palette(&self)->wezterm_term::color::ColorPalette {Default::default()}}
    let endpoint=std::env::var("REX_TEST_ENDPOINT")?;
    let (client,_events)=Client::connect(endpoint.clone()).await?;
    let created=client.call("session.create",json!({"label":"wezterm-rex-native-test","initial_windows":[{"layout":{"block":{"flavor":"com.superlogical.terminal.shell","options":{"shell":"none","login":false,"command":["/bin/sh"]}}}}]})).await?;
    let session=created["session_id"].as_str().context("session")?.to_owned();
    let block=created["initial_windows"][0]["block_ids"][0].as_str().context("block")?.to_owned();
    let result=async {
        client.call("session.attach",json!({"session_id":session})).await?;
        client.call("session.own_block",json!({"session_id":session,"block_ids":[block],"mode":"take"})).await?;
        let actor=Actor::start_with_config_ready(endpoint,client.clone(),session.clone(),block.clone(),std::sync::Arc::new(TestConfig),|_|{},wait_ready).await?;
        client.call("block.event",json!({"session_id":session,"block_id":block,"event":{"type":"paste","payload":{"text":"i=1; while [ $i -le 150 ]; do printf 'NATIVE_%03d\\n' $i; i=$((i+1)); done; printf '\\033[31mRED_界_é\\033[0m\\nNATIVE_DONE\\n'\n"}}})).await?;
        let deadline=std::time::Instant::now()+Duration::from_secs(10);
        loop {
            let f=actor.frame.lock().unwrap().clone();
            let (first,rows)=actor.lines(f.base..f.base+f.total as isize);
            let text=rows.iter().map(|r|r.as_str().to_string()).collect::<Vec<_>>().join("\n");
            if text.lines().any(|l|l.trim()=="NATIVE_DONE") {
                ensure!(first==f.base,"history starts at wrong row");
                let nums=text.lines().filter_map(|l|l.trim().strip_prefix("NATIVE_")?.parse::<usize>().ok()).collect::<Vec<_>>();
                ensure!(nums==(1..=150).collect::<Vec<_>>(),"history omitted/reordered rows");
                let row=rows.iter().find(|r|r.as_str().to_string().trim_start().starts_with("RED_界_é")).context("Unicode row")?;
                ensure!(row.visible_cells().any(|c|c.str()=="界" && c.width()==2),"wide cell lost");
                ensure!(row.visible_cells().any(|c|c.str()=="é"),"combining grapheme lost");
                ensure!(row.visible_cells().next().unwrap().attrs().foreground()==termwiz::color::ColorAttribute::PaletteIndex(1),"color lost");
                break;
            }
            ensure!(std::time::Instant::now()<deadline,"native output timed out: {text}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // Local feedback is immediate, and an older server size in flight
        // cannot move the renderer backwards during a drag.
        actor.resize_local(wezterm_term::TerminalSize {cols:117,rows:37,..Default::default()});
        let local=actor.frame.lock().unwrap().clone();
        ensure!((local.cols,local.rows)==(117,37),"local resize waited for server");
        client.call("com.superlogical.terminal.resize",json!({"session_id":session,"block_id":block,"args":{"columns":81,"rows":25}})).await?;
        client.call("com.superlogical.terminal.write",json!({"session_id":session,"block_id":block,"args":{"data":"printf 'RESIZE_BARRIER\\n'\n"}})).await?;
        let deadline=std::time::Instant::now()+Duration::from_secs(5);
        loop {
            let f=actor.frame.lock().unwrap().clone();
            ensure!((f.cols,f.rows)==(117,37),"stale remote resize moved the local renderer backwards");
            let (_,rows)=actor.lines(f.base..f.base+f.total as isize);
            if rows.iter().any(|r|r.as_str().trim()=="RESIZE_BARRIER") {break;}
            ensure!(std::time::Instant::now()<deadline,"resize barrier timed out");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        client.call("com.superlogical.terminal.resize",json!({"session_id":session,"block_id":block,"args":{"columns":117,"rows":37}})).await?;
        // A subsequent external resize proves that the matching ack released
        // the local override instead of permanently ignoring server sizes.
        client.call("com.superlogical.terminal.resize",json!({"session_id":session,"block_id":block,"args":{"columns":95,"rows":31}})).await?;
        for _ in 0..100 {if actor.frame.lock().unwrap().cols==95{break}tokio::time::sleep(Duration::from_millis(20)).await;}
        let f=actor.frame.lock().unwrap();ensure!((f.cols,f.rows)==(95,31),"native resize not applied");
        Ok::<_,anyhow::Error>(())
    }.await;
    client.call("session.destroy",json!({"session_id":session})).await?;
    result
}

#[tokio::test]
#[ignore = "requires REX_TEST_ENDPOINT pointing to an isolated server"]
async fn reconnect_control_and_native_stream_without_replaying_input() -> Result<()> {
    use wezterm_rex::actor::Actor;
    let target=std::env::var("REX_TEST_ENDPOINT")?;
    let target=target.strip_prefix("unix://").context("proxy test needs Unix server")?.to_owned();
    let path=format!("/tmp/wezterm-rex-reconnect-{}.sock",std::process::id());
    let listener=tokio::net::UnixListener::bind(&path)?;
    let (disconnect,revision)=tokio::sync::watch::channel(0u64);
    let proxy=tokio::spawn(async move {
        loop {
            let (mut incoming,_)=listener.accept().await?;
            let mut upstream=tokio::net::UnixStream::connect(&target).await?;
            let mut revision=revision.clone();revision.borrow_and_update();
            tokio::spawn(async move {tokio::select! {
                _=tokio::io::copy_bidirectional(&mut incoming,&mut upstream)=>{},
                _=revision.changed()=>{},
            }});
        }
        #[allow(unreachable_code)] Ok::<(),std::io::Error>(())
    });
    let endpoint=format!("unix://{path}");
    let (client,events)=Client::connect(endpoint.clone()).await?;
    let created=client.call("session.create",json!({"label":"wezterm-reconnect-test","initial_windows":[{"layout":{"block":{"flavor":"com.superlogical.terminal.shell","options":{"shell":"none","login":false,"command":["/bin/sh"]}}}}]})).await?;
    let session=created["session_id"].as_str().context("session")?.to_owned();
    let block=created["initial_windows"][0]["block_ids"][0].as_str().context("block")?.to_owned();
    let result=async {
        client.call("session.attach",json!({"session_id":session})).await?;
        client.call("session.own_block",json!({"session_id":session,"block_ids":[block],"mode":"take"})).await?;
        let actor=Actor::start(endpoint,client.clone(),session.clone(),block.clone(),|_|{}).await?;
        let write=|data:&str|json!({"session_id":session,"block_id":block,"args":{"data":data}});
        client.call("com.superlogical.terminal.write",write("n=0; n=$((n+1)); printf 'REX_ONCE_%s\\n' $n\n")).await?;
        tokio::time::sleep(Duration::from_millis(100)).await;
        disconnect.send(1)?;
        let event=tokio::time::timeout(Duration::from_secs(15),async {
            loop {let e=events.recv_async().await?;if e["type"]=="reconnected"{return Ok::<_,anyhow::Error>(e)}}
        }).await??;
        ensure!(event["resumed"]==true,"control client did not resume");
        client.call("session.attach",json!({"session_id":session})).await?;
        client.call("com.superlogical.terminal.write",write("printf 'REX_AFTER_%s\\n' $n\n")).await?;
        let end=std::time::Instant::now()+Duration::from_secs(10);
        loop {
            let f=actor.frame.lock().unwrap().clone();
            let (_,rows)=actor.lines(f.base..f.base+f.total as isize);
            let text=rows.into_iter().map(|r|r.as_str().to_string()).collect::<Vec<_>>().join("\n");
            if text.lines().any(|l|l.trim()=="REX_AFTER_1") {
                ensure!(text.lines().filter(|l|l.trim()=="REX_ONCE_1").count()==1,"pre-disconnect input/output duplicated");
                ensure!(f.connected,"native stream stayed disconnected");break;
            }
            ensure!(std::time::Instant::now()<end,"native stream did not resume: {text}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok::<_,anyhow::Error>(())
    }.await;
    client.call("session.destroy",json!({"session_id":session})).await?;
    drop(client);proxy.abort();std::fs::remove_file(path)?;result
}

#[tokio::test]
#[ignore = "requires REX_TEST_ENDPOINT pointing to an isolated server"]
async fn uploaded_file_batch_preserves_bytes_and_quotes_paths() -> Result<()> {
    use wezterm_rex::{actor::Actor,upload};
    let endpoint=std::env::var("REX_TEST_ENDPOINT")?;
    let (client,_events)=Client::connect(endpoint.clone()).await?;
    let created=client.call("session.create",json!({"label":"wezterm-rex-upload-test","initial_windows":[{"layout":{"block":{"flavor":"com.superlogical.terminal.shell","options":{"shell":"none","login":false,"command":["/bin/sh"]}}}}]})).await?;
    let session=created["session_id"].as_str().context("session")?.to_owned();
    let block=created["initial_windows"][0]["block_ids"][0].as_str().context("block")?.to_owned();
    let result=async {
        client.call("session.attach",json!({"session_id":session})).await?;
        client.call("session.own_block",json!({"session_id":session,"block_ids":[block],"mode":"take"})).await?;
        let actor=Actor::start(endpoint.clone(),client.clone(),session.clone(),block.clone(),|_|{}).await?;
        // The command reads precisely the paths Rex pastes, then removes its own fixtures.
        let command="python3 -c 'import sys,hashlib,os; print(\"REX_UPLOAD_HASHES \"+\" \".join(hashlib.sha256(open(p,\"rb\").read()).hexdigest() for p in sys.argv[1:])); [os.unlink(p) for p in sys.argv[1:]]' ";
        client.call("com.superlogical.terminal.write",json!({"session_id":session,"block_id":block,"args":{"data":command}})).await?;
        let content=[vec![0x91;70_001],(0u8..=255).collect::<Vec<_>>()];
        let expected=format!("REX_UPLOAD_HASHES {}",content.iter().map(|b|ring::digest::digest(&ring::digest::SHA256,b).as_ref().iter().map(|b|format!("{b:02x}")).collect::<String>()).collect::<Vec<_>>().join(" "));
        let files=[upload::File::bytes("space ' quoted.bin",content[0].clone()),upload::File::bytes("unicode-界.bin",content[1].clone())];
        let batch=upload::new_uuid()?;let mut tickets=vec![];
        for (index,file) in files.iter().enumerate() {
            let r=client.call("com.superlogical.terminal.upload_begin",json!({"session_id":session,"block_id":block,"args":{"ref":format!("upload:{}",upload::new_uuid()?),"name":file.name,"size":file.size()?,"batch":batch,"index":index,"count":2}})).await?;
            tickets.push(r["ticket"].as_str().context("ticket")?.to_owned());
        }
        let (a,b)=tokio::join!(upload::send(&endpoint,&tickets[0],&files[0],files[0].size()?),upload::send(&endpoint,&tickets[1],&files[1],files[1].size()?));a?;b?;
        client.call("block.event",json!({"session_id":session,"block_id":block,"event":{"type":"key","payload":{"key":"enter","action":"press","mods":""}}})).await?;
        let deadline=std::time::Instant::now()+Duration::from_secs(10);
        loop {
            let f=actor.frame.lock().unwrap().clone();let (_,rows)=actor.lines(f.base..f.base+f.total as isize);
            // Shell output can wrap; use the server's unwrapped text to verify all bytes.
            let text=client.call("com.superlogical.terminal.format",json!({"session_id":session,"block_id":block,"args":{"format":"text","unwrap":true}})).await?;
            if text["content"].as_str().unwrap_or("").lines().any(|l|l.trim()==expected){break}
            ensure!(std::time::Instant::now()<deadline,"upload hash check timed out: {} rows, {}",rows.len(),text);
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        Ok::<_,anyhow::Error>(())
    }.await;
    let cleanup=client.call("session.destroy",json!({"session_id":session})).await;
    result?;cleanup?;Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires an explicitly selected isolated Rex server"]
async fn server_lua_buffer_is_shared_and_persists() -> Result<()> {
    let endpoint=std::env::var("REX_TEST_ENDPOINT")?;
    let (client,events)=Client::connect(endpoint.clone()).await?;
    let created=client.call("session.create",json!({"label":"rex-client-buffer-test"})).await?;
    let session=created["session_id"].as_str().context("session ID")?;
    client.call("session.attach",json!({"session_id":session})).await?;
    let result=async {
        let begin=client.call("client.action",json!({"action":"wezterm_buffer_begin","session_id":session})).await;
        for e in events.try_iter() { eprintln!("Event kind {}",e["payload"]["type"]); }
        let begin=begin?;
        let token=begin["result"]["token"].as_str().context("copy token")?;
        let expected="界é quote ' \\\n".repeat(9000);
        for (index,chunk) in expected.chars().collect::<Vec<_>>().chunks(1000).enumerate() {
            let text=chunk.iter().collect::<String>();
            let finish=(index+1)*1000>=expected.chars().count();
            client.call("client.action",json!({"action":"wezterm_buffer_append","session_id":session,"args":{"token":token,"text":text,"finish":finish}})).await?;
            if index==2 {client.call("server.config_reload",json!({})).await?;}
        }
        let (other,_events)=Client::connect(endpoint).await?;
        other.call("session.attach",json!({"session_id":session})).await?;
        let mut actual=String::new();
        loop {
            let response=other.call("client.action",json!({"action":"wezterm_buffer_read","session_id":session,"args":{"offset":actual.len()}})).await?;
            actual.push_str(response["result"]["text"].as_str().context("copy text")?);
            if response["result"]["eof"]==true {break;}
        }
        ensure!(actual==expected,"buffer changed across client connections");
        Ok::<_,anyhow::Error>(())
    }.await;
    client.call("session.destroy",json!({"session_id":session})).await?;
    result
}
