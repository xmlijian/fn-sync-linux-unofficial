// 任务页 — 任务列表 + 创建/暂停/恢复/删除（官方 Tasks 页语义）
import { useState } from "react";
import { commands, type NasAccount, type Task, formatBytes, formatTime } from "../lib/commands";
import { t } from "../i18n";

interface Props {
  tasks: Task[];
  accounts: NasAccount[];
  onChanged: () => void;
  statusLabel: (s: number) => string;
  modeLabel: (m: string) => string;
}

interface TaskFormState {
  name: string;
  nas_acc_id: number;
  mode: "bisynchronous" | "uploadonly" | "downloadonly";
  nas_path: string;
  pc_path: string;
}

export default function TasksPage({ tasks, accounts, onChanged, statusLabel, modeLabel }: Props) {
  const [showCreate, setShowCreate] = useState(false);
  const [form, setForm] = useState<TaskFormState>({
    name: "",
    nas_acc_id: accounts[0]?.id ?? 0,
    mode: "bisynchronous",
    nas_path: "/vol1/",
    pc_path: "",
  });
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState("");

  const statusClass = (s: number) =>
    s === 4 ? "completed" : s === 3 ? "paused" : [5, 7, 9, 10, 11, 12, 17].includes(s) ? "failed" : "";

  const pickFolder = async () => {
    const { open } = await import("@tauri-apps/plugin-dialog");
    const picked = await open({ directory: true, multiple: false });
    if (typeof picked === "string") {
      setForm((f) => ({ ...f, pc_path: picked }));
    }
  };

  const createTask = async () => {
    setBusy(true);
    setErr("");
    try {
      const acc = accounts.find((a) => a.id === form.nas_acc_id);
      if (!acc) throw new Error("no account");
      await commands.validateAppName(form.name);
      const uuid = Date.now().toString();
      const id = await commands.addTask({
        name: form.name,
        address: acc.address,
        conn_type: acc.conn_type,
        uuid,
        identifier: `tieren.SyncFile-${acc.username}-${acc.address}`,
        mode: form.mode,
        nas_path: form.nas_path,
        pc_path: form.pc_path,
        status: 0,
        nas_acc_id: acc.id,
        is_init_sync: 0,
        open_cloud: 0,
        filter_rule: {
          ignore_dotfiles: 1,
          enable_ignore_ext: 1,
          ignore_extensions: ["lnk", "pst", "swp"],
          enable_size_lower: 0,
          size_lower_bound: null,
          enable_size_upper: 0,
          size_upper_bound: null,
        },
      });
      await commands.reloadTask(id);
      setShowCreate(false);
      setForm((f) => ({ ...f, name: "", pc_path: "" }));
      onChanged();
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  const groups = new Map<number, { acc?: NasAccount; tasks: Task[] }>();
  for (const task of tasks) {
    const g = groups.get(task.nas_acc_id) ?? { acc: accounts.find((a) => a.id === task.nas_acc_id), tasks: [] };
    g.tasks.push(task);
    groups.set(task.nas_acc_id, g);
  }

  return (
    <div className="page">
      <div style={{ display: "flex", justifyContent: "space-between", marginBottom: 16 }}>
        <span />
        <button className="btn-primary" onClick={() => setShowCreate(true)}>
          {t("task.createSyncTask", "Create Task")}
        </button>
      </div>

      {tasks.length === 0 ? (
        <div className="empty">
          <div style={{ fontSize: 40 }}>⇄</div>
          <div>{t("sync.noTasks", "No sync tasks yet")}</div>
        </div>
      ) : (
        <div className="task-list">
          {[...groups.entries()].map(([accId, g]) => (
            <div key={accId}>
              <div className="task-group-title">
                {g.acc?.nas_name ?? "NAS"} · {g.acc?.address}
              </div>
              {g.tasks.map((task) => (
                <div className="task-card" key={task.id}>
                  <div className="task-icon">
                    {{ bisynchronous: "⇄", uploadonly: "↑", downloadonly: "↓" }[task.mode] ?? "⇄"}
                  </div>
                  <div className="task-main">
                    <div className="task-name">{task.name}</div>
                    <div className="task-paths">
                      {task.pc_path} ⇄ {task.nas_path} · {modeLabel(task.mode)} ·{" "}
                      {formatTime(task.updated_at)}
                    </div>
                  </div>
                  <span className={`status-pill ${statusClass(task.status)}`}>
                    {statusLabel(task.status)}
                  </span>
                  <div className="task-actions">
                    {task.status === 3 ? (
                      <button className="btn" onClick={async () => { await commands.resumeTask(task.id); onChanged(); }}>
                        {t("action.resume", "Resume")}
                      </button>
                    ) : (
                      <button className="btn" onClick={async () => { await commands.pauseTask(task.id); onChanged(); }}>
                        {t("action.pause", "Pause")}
                      </button>
                    )}
                    <button
                      className="btn btn-danger"
                      onClick={async () => {
                        if (!confirm(t("task.deleteConfirm", "Delete this task? Synced files are kept."))) return;
                        await commands.deleteTask(task.id);
                        onChanged();
                      }}
                    >
                      {t("task.delete", "Delete Task")}
                    </button>
                  </div>
                </div>
              ))}
            </div>
          ))}
        </div>
      )}

      {showCreate && (
        <div className="modal-mask" onClick={() => setShowCreate(false)}>
          <div className="modal" onClick={(e) => e.stopPropagation()}>
            <h3>{t("task.createSyncTask", "Create Task")}</h3>
            <input
              type="text"
              placeholder={t("task.name", "Task name")}
              value={form.name}
              onChange={(e) => setForm((f) => ({ ...f, name: e.target.value }))}
            />
            <select
              value={form.nas_acc_id}
              onChange={(e) => setForm((f) => ({ ...f, nas_acc_id: Number(e.target.value) }))}
            >
              {accounts.map((a) => (
                <option key={a.id} value={a.id}>
                  {a.nas_name} ({a.address})
                </option>
              ))}
            </select>
            <select
              value={form.mode}
              onChange={(e) => setForm((f) => ({ ...f, mode: e.target.value as TaskFormState["mode"] }))}
            >
              <option value="bisynchronous">{t("sync.mode.twoWay", "Two-Way Sync")}</option>
              <option value="uploadonly">{t("sync.mode.uploadOnly", "Upload Only")}</option>
              <option value="downloadonly">{t("sync.mode.downloadOnly", "Download Only")}</option>
            </select>
            <input
              type="text"
              placeholder="NAS 路径，如 /vol1/photos"
              value={form.nas_path}
              onChange={(e) => setForm((f) => ({ ...f, nas_path: e.target.value }))}
            />
            <div style={{ display: "flex", gap: 8 }}>
              <input
                type="text"
                placeholder={t("task.checkPcPathFailed", "PC path")}
                value={form.pc_path}
                onChange={(e) => setForm((f) => ({ ...f, pc_path: e.target.value }))}
              />
              <button className="btn btn-ghost" onClick={pickFolder}>
                {t("action.selectFolder", "Select Folder")}
              </button>
            </div>
            <div className="login-err">{err}</div>
            <div className="modal-row">
              <button className="btn btn-ghost" onClick={() => setShowCreate(false)}>
                {t("action.cancel", "Cancel")}
              </button>
              <button className="btn-primary" disabled={busy} onClick={createTask}>
                {busy ? t("action.creating", "Creating") : t("action.create", "Create")}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

// 保留对 formatBytes 的引用避免 tree-shake 报错（设置页全局限速使用）
void formatBytes;
