use std::path::Path;
use std::sync::Arc;

use fdm_core::SettingsStore;
use fdm_engine::{AddTaskResult, TaskManager};
use serde_json::json;
use tempfile::tempdir;

fn make_manager(root: &Path) -> Arc<TaskManager> {
    let settings = Arc::new(SettingsStore::load(root).unwrap());
    TaskManager::new(Some(root.to_path_buf()), settings).unwrap()
}

#[tokio::test]
async fn duplicate_download_policies() {
    let root = tempdir().unwrap();
    let downloads = root.path().join("downloads");
    std::fs::create_dir_all(&downloads).unwrap();
    let manager = make_manager(root.path());

    let base = json!({
        "url": "https://example.com/files/file.bin",
        "dir": downloads.to_string_lossy(),
        "filename": "file.bin",
        "start": false,
        "connections": 4
    });
    let first = match manager.add_task_checked(&base).await.unwrap() {
        AddTaskResult::Added(snap) => snap,
        other => panic!("expected added, got {other:?}"),
    };
    assert_eq!(first.filename, "file.bin");

    // 未记住策略：必须返回 NeedsChoice
    let needs = manager.add_task_checked(&base).await.unwrap();
    match needs {
        AddTaskResult::NeedsChoice {
            existing,
            default_action,
        } => {
            assert_eq!(existing.id, first.id);
            assert_eq!(default_action, 3);
        }
        other => panic!("expected needs choice, got {other:?}"),
    }

    // 带序号另存
    let mut numbered_opts = base.clone();
    numbered_opts["duplicateAction"] = json!(1);
    let numbered = match manager.add_task_checked(&numbered_opts).await.unwrap() {
        AddTaskResult::Added(snap) => snap,
        other => panic!("expected numbered add, got {other:?}"),
    };
    assert_eq!(numbered.filename, "file (1).bin");

    // 覆盖：复用已有任务的目标文件名
    let mut overwrite_opts = base.clone();
    overwrite_opts["duplicateAction"] = json!(2);
    let overwritten = match manager.add_task_checked(&overwrite_opts).await.unwrap() {
        AddTaskResult::Added(snap) => snap,
        other => panic!("expected overwrite add, got {other:?}"),
    };
    assert_eq!(overwritten.filename, "file.bin");

    // 复用已有任务
    let mut resume_opts = base.clone();
    resume_opts["duplicateAction"] = json!(3);
    match manager.add_task_checked(&resume_opts).await.unwrap() {
        AddTaskResult::Existing { task, action } => {
            assert_eq!(task.id, first.id);
            assert_eq!(action, 3);
        }
        other => panic!("expected existing, got {other:?}"),
    }

    // 跳过
    let mut skip_opts = base.clone();
    skip_opts["duplicateAction"] = json!(0);
    match manager.add_task_checked(&skip_opts).await.unwrap() {
        AddTaskResult::Existing { task, action } => {
            assert_eq!(task.id, first.id);
            assert_eq!(action, 0);
        }
        other => panic!("expected skipped, got {other:?}"),
    }

    // 同一 URL 换文件名 -> 不算重复
    let mut renamed_opts = base.clone();
    renamed_opts["filename"] = json!("other.bin");
    match manager.add_task_checked(&renamed_opts).await.unwrap() {
        AddTaskResult::Added(snap) => assert_eq!(snap.filename, "other.bin"),
        other => panic!("expected added for different filename, got {other:?}"),
    }
}

#[tokio::test]
async fn remembered_duplicate_policy() {
    let root = tempdir().unwrap();
    let downloads = root.path().join("downloads");
    std::fs::create_dir_all(&downloads).unwrap();
    let manager = make_manager(root.path());
    manager
        .settings
        .update_patch(&json!({"duplicateAction": 1, "rememberDuplicateAction": true}))
        .unwrap();

    let opts = json!({
        "url": "https://example.com/a.zip",
        "dir": downloads.to_string_lossy(),
        "filename": "a.zip",
        "start": false
    });
    let first = match manager.add_task_checked(&opts).await.unwrap() {
        AddTaskResult::Added(snap) => snap,
        other => panic!("expected added, got {other:?}"),
    };
    assert_eq!(first.filename, "a.zip");

    // 设置已记住：不再询问，直接按动作 1 生成带序号名
    let second = match manager.add_task_checked(&opts).await.unwrap() {
        AddTaskResult::Added(snap) => snap,
        other => panic!("expected remembered numbered add, got {other:?}"),
    };
    assert_eq!(second.filename, "a (1).zip");
}

#[tokio::test]
async fn disk_name_conflict_suggests_numbered_name() {
    let root = tempdir().unwrap();
    let downloads = root.path().join("downloads");
    std::fs::create_dir_all(&downloads).unwrap();
    std::fs::write(downloads.join("report.pdf"), b"existing").unwrap();
    let manager = make_manager(root.path());

    let opts = json!({
        "url": "https://example.com/report.pdf",
        "dir": downloads.to_string_lossy(),
        "filename": "report.pdf",
        "start": false
    });
    match manager.add_task_checked(&opts).await.unwrap() {
        AddTaskResult::NameConflict {
            reason, suggested, ..
        } => {
            assert_eq!(reason, "disk");
            assert_eq!(suggested, "report (1).pdf");
        }
        other => panic!("expected name conflict, got {other:?}"),
    }

    let mut numbered = opts.clone();
    numbered["fileAction"] = json!(1);
    match manager.add_task_checked(&numbered).await.unwrap() {
        AddTaskResult::Added(snap) => assert_eq!(snap.filename, "report (1).pdf"),
        other => panic!("expected numbered add, got {other:?}"),
    }
}
