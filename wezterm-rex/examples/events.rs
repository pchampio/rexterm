use serde_json::json;
#[tokio::main(flavor="current_thread")]
async fn main()->anyhow::Result<()> {
 let (c,e)=wezterm_rex::transport::Client::connect(std::env::var("REX_TEST_ENDPOINT")?).await?;
 let v=c.call("session.create",json!({"label":"rex-event-test"})).await?;let s=&v["session_id"];
 let r=c.call_reply("session.attach",json!({"session_id":s})).await?;println!("attach seq {}",r.seq);
 c.call("session.set_label",json!({"session_id":s,"label":"rex-event-test-renamed"})).await?;
 tokio::time::sleep(std::time::Duration::from_millis(100)).await;
 for event in e.try_iter(){println!("{event}");}
 c.call("session.destroy",json!({"session_id":s})).await?;Ok(())
}
