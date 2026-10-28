use super::*;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn delayed_client(short_reads: bool) -> (SftpClient, tokio::task::JoinHandle<()>) {
    let (client, mut server) = tokio::io::duplex(2 * SFTP_TRANSFER_BATCH_BYTES);
    let task = tokio::spawn(async move {
        let n = server.read_u32().await.unwrap();
        let mut init = vec![0; n as usize];
        server.read_exact(&mut init).await.unwrap();
        assert_eq!(init[0], 1);
        server
            .write_all(&[0, 0, 0, 5, 2, 0, 0, 0, 3])
            .await
            .unwrap();
        let (mut reader, writer) = tokio::io::split(server);
        let writer = Arc::new(tokio::sync::Mutex::new(writer));
        let mut tasks = tokio::task::JoinSet::new();
        while let Ok(n) = reader.read_u32().await {
            let mut packet = vec![0; n as usize];
            reader.read_exact(&mut packet).await.unwrap();
            let writer = writer.clone();
            tasks.spawn(async move {
                let handle_len = u32::from_be_bytes(packet[5..9].try_into().unwrap()) as usize;
                let start = 9 + handle_len;
                let offset = u64::from_be_bytes(packet[start..start + 8].try_into().unwrap());
                let n =
                    u32::from_be_bytes(packet[start + 8..start + 12].try_into().unwrap()) as usize;
                let mut reply = Vec::new();
                if packet[0] == 5 {
                    // Vary the delay so replies arrive out of order.
                    tokio::time::sleep(Duration::from_millis(8 + offset % 3)).await;
                    let n = if short_reads { n.min(1000) } else { n };
                    reply.push(103);
                    reply.extend_from_slice(&packet[1..5]);
                    reply.extend_from_slice(&(n as u32).to_be_bytes());
                    reply.extend((0..n).map(|i| ((offset + i as u64) % 251) as u8));
                } else {
                    assert_eq!(packet[0], 6);
                    assert_eq!(packet.len(), start + 12 + n);
                    tokio::time::sleep(Duration::from_millis(8)).await;
                    reply.push(101);
                    reply.extend_from_slice(&packet[1..5]);
                    // One rejected write must not abandon other issued writes.
                    reply.extend_from_slice(&(if offset == 0 { 4u32 } else { 0 }).to_be_bytes());
                    reply.extend_from_slice(&[0; 8]);
                }
                let mut writer = writer.lock().await;
                writer.write_u32(reply.len() as u32).await.unwrap();
                writer.write_all(&reply).await.unwrap();
            });
        }
        while let Some(result) = tasks.join_next().await {
            result.unwrap();
        }
    });
    (SftpClient::connect(client).await.unwrap(), task)
}

#[tokio::test]
async fn pipelining_reorders_replies_and_removes_per_packet_round_trip_waits() {
    let (client, server) = delayed_client(false).await;
    let length = 16 * SFTP_TRANSFER_CHUNK_BYTES;
    let start = Instant::now();
    let mut serial = Vec::new();
    for offset in (0..length).step_by(SFTP_TRANSFER_CHUNK_BYTES) {
        serial.extend(
            client
                .read_range("file", offset as u64, SFTP_TRANSFER_CHUNK_BYTES)
                .await
                .unwrap(),
        );
    }
    let serial_time = start.elapsed();
    let start = Instant::now();
    let pipelined = client.read_range("file", 0, length).await.unwrap();
    let pipeline_time = start.elapsed();
    assert_eq!(pipelined, serial);
    assert_eq!(
        pipelined,
        (0..length).map(|i| (i % 251) as u8).collect::<Vec<_>>()
    );
    assert!(
        pipeline_time * 2 < serial_time,
        "serial {serial_time:?}, pipeline {pipeline_time:?}"
    );
    println!(
        "synthetic 8-10 ms response delay, 512 KiB: serial={serial_time:?}, pipeline={pipeline_time:?}"
    );
    client.close().await.unwrap();
    server.abort();
}

#[tokio::test]
async fn pipeline_accepts_short_reads_and_rejects_unbounded_ranges() {
    let (client, server) = delayed_client(true).await;
    let bytes = client.read_range("file", 123, 65_537).await.unwrap();
    assert_eq!(
        bytes,
        (123..123 + 65_537)
            .map(|i| (i % 251) as u8)
            .collect::<Vec<_>>()
    );
    assert!(client.read_range("file", u64::MAX, 1).await.is_err());
    assert!(
        client
            .read_range("file", 0, SFTP_TRANSFER_BATCH_BYTES + 1)
            .await
            .is_err()
    );
    assert!(
        client
            .write_range("file", 0, &vec![0; SFTP_TRANSFER_BATCH_BYTES + 1])
            .await
            .is_err()
    );
    assert!(
        client
            .write_range("file", 0, &vec![0; SFTP_TRANSFER_BATCH_BYTES])
            .await
            .is_err()
    );
    // The response stream remains usable after the rejected batch was drained.
    assert_eq!(client.read_range("file", 0, 1).await.unwrap(), vec![0]);
    client.close().await.unwrap();
    server.abort();
}
