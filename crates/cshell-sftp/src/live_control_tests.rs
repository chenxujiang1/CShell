use super::*;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

#[tokio::test]
async fn live_pause_finishes_metadata_write_and_releases_its_lock_before_parking() {
    let control = TransferControl::default();
    let copy_control = control.clone();
    let lock = Arc::new(tokio::sync::Mutex::new(0));
    let copy_lock = lock.clone();
    let started = Arc::new(tokio::sync::Notify::new());
    let copy_started = started.clone();
    let finish_write = Arc::new(tokio::sync::Notify::new());
    let copy_finish = finish_write.clone();
    let task = tokio::spawn(async move {
        copy_control
            .run(async {
                {
                    let _critical = copy_control.enter_critical();
                    let mut guard = copy_lock.lock().await;
                    copy_started.notify_one();
                    copy_finish.notified().await;
                    *guard = 1;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
                99
            })
            .await
    });
    started.notified().await;
    control.pause();
    assert!(!control.is_paused(), "checkpoint is still finishing");
    finish_write.notify_one();
    let guard = tokio::time::timeout(Duration::from_secs(1), lock.lock())
        .await
        .expect("pause must release metadata lock");
    assert_eq!(*guard, 1);
    drop(guard);
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(control.is_paused());
    assert!(!task.is_finished());
    control.resume();
    assert_eq!(task.await.unwrap(), 99);
}

#[tokio::test]
async fn live_pause_keeps_one_copy_future_and_cancel_wakes_a_paused_task() {
    let control = TransferControl::default();
    let progress = Arc::new(AtomicUsize::new(0));
    let task_control = control.clone();
    let task_progress = progress.clone();
    let task = tokio::spawn(async move {
        task_control
            .run(async {
                let mut output = Vec::new();
                for value in 0..64u8 {
                    tokio::time::sleep(Duration::from_millis(3)).await;
                    if task_control.is_cancelled() {
                        break;
                    }
                    output.extend_from_slice(&[value; 128]);
                    task_progress.fetch_add(1, Ordering::SeqCst);
                }
                output
            })
            .await
    });
    while progress.load(Ordering::SeqCst) < 4 {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    control.pause();
    tokio::time::sleep(Duration::from_millis(15)).await;
    let at_pause = progress.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(at_pause, progress.load(Ordering::SeqCst));
    assert!(!control.is_cancelled());
    control.resume();
    let output = tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        output,
        (0..64u8).flat_map(|value| [value; 128]).collect::<Vec<_>>()
    );
    control.pause();
    let cancelled_control = control.clone();
    let task = tokio::spawn(async move {
        cancelled_control
            .run(async { cancelled_control.is_cancelled() })
            .await
    });
    tokio::task::yield_now().await;
    assert!(!task.is_finished());
    control.cancel();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
    );
}

#[tokio::test]
async fn live_pause_before_initialization_prevents_the_first_poll() {
    let control = TransferControl::default();
    control.pause();
    let copy_control = control.clone();
    let polled = Arc::new(AtomicBool::new(false));
    let task_polled = polled.clone();
    let task = tokio::spawn(async move {
        copy_control
            .run(async {
                task_polled.store(true, Ordering::SeqCst);
                17
            })
            .await
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!polled.load(Ordering::SeqCst));
    control.resume();
    assert_eq!(task.await.unwrap(), 17);
}
