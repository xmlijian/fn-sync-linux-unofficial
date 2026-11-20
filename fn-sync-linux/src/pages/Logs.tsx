// 同步日志页 — 查询/过滤/清空（官方 Logs 页语义）
import { useCallback, useEffect, useState } from "react";
import { commands, ActionCode, formatTime, type LogEntry } from "../lib/commands";
import { t } from "../i18n";

const PAGE_SIZE = 100;

export default function LogsPage() {
  const [rows, setRows] = useState<LogEntry[]>([]);
  const [total, setTotal] = useState(0);
  const [keyword, setKeyword] = useState("");
  const [actionFilter, setActionFilter] = useState(-100);
  const [statusFilter, setStatusFilter] = useState(-100);
  const [page, setPage] = useState(0);

  const load = useCallback(async () => {
    try {
      const res = await commands.queryLogs({
        keyword: keyword || undefined,
        action_code: actionFilter === -100 ? undefined : actionFilter,
        status: statusFilter === -100 ? undefined : statusFilter,
        page_size: PAGE_SIZE,
        page,
      });
      setRows(res.data);
      setTotal(res.total);
    } catch {}
  }, [keyword, actionFilter, statusFilter, page]);

  useEffect(() => {
    load();
    const timer = setInterval(load, 3000);
    return () => clearInterval(timer);
  }, [load]);

  const actionLabel = (code: number): string => {
    switch (code) {
      case ActionCode.Upload: return t("status.uploaded", "Uploaded");
      case ActionCode.Download: return t("status.downloaded", "Downloaded");
      case ActionCode.Delete: return t("action.delete", "Delete");
      case ActionCode.Rename: return t("action.rename", "Rename");
      case ActionCode.Exception: return t("status.error", "Error");
      case ActionCode.MetaCreate: return t("log.metaCreate", "Metadata downloaded");
      case ActionCode.Move: return t("action.move", "Move");
      default: return String(code);
    }
  };

  const pages = Math.max(1, Math.ceil(total / PAGE_SIZE));

  return (
    <div className="page">
      <div style={{ display: "flex", gap: 8, marginBottom: 14, alignItems: "center" }}>
        <input
          type="text"
          style={{ width: 240 }}
          placeholder={t("search.fileName", "Search File")}
          value={keyword}
          onChange={(e) => { setKeyword(e.target.value); setPage(0); }}
        />
        <select
          style={{ width: 140 }}
          value={actionFilter}
          onChange={(e) => { setActionFilter(Number(e.target.value)); setPage(0); }}
        >
          <option value={-100}>{t("log.filter.all", "All")}</option>
          {[1, 2, 3, 4, 5, 6, 7].map((c) => (
            <option key={c} value={c}>{actionLabel(c)}</option>
          ))}
        </select>
        <select
          style={{ width: 140 }}
          value={statusFilter}
          onChange={(e) => { setStatusFilter(Number(e.target.value)); setPage(0); }}
        >
          <option value={-100}>{t("log.filter.all", "All")}</option>
          <option value={1}>{t("status.success", "Success")}</option>
          <option value={0}>{t("status.failed", "Failed")}</option>
        </select>
        <span style={{ flex: 1 }} />
        <button
          className="btn btn-danger"
          onClick={async () => {
            if (confirm(t("action.clearAllLogs", "Clear all logs?"))) {
              await commands.clearAllLogs();
              load();
            }
          }}
        >
          {t("action.clearAllLogs", "Clear all logs")}
        </button>
      </div>

      {rows.length === 0 ? (
        <div className="empty">
          <div>{t("log.noLogs", "No logs")}</div>
        </div>
      ) : (
        <table className="log-table">
          <thead>
            <tr>
              <th>{t("log.time", "Time")}</th>
              <th>{t("log.task", "Task")}</th>
              <th>{t("log.event", "Event")}</th>
              <th>{t("log.fileName", "File Name")}</th>
              <th>{t("log.status", "Status")}</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((r) => (
              <tr key={r.id}>
                <td>{formatTime(r.created_at)}</td>
                <td>{r.sync_folder}</td>
                <td>{actionLabel(r.action_code)}</td>
                <td title={r.path}>{r.name || r.path}</td>
                <td className={r.status === 1 ? "log-ok" : "log-fail"}>
                  {r.status === 1
                    ? t("status.success", "Success")
                    : r.failed_text || t("status.failed", "Failed")}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}

      {pages > 1 && (
        <div style={{ display: "flex", gap: 8, marginTop: 12, justifyContent: "center" }}>
          <button className="btn btn-ghost" disabled={page === 0} onClick={() => setPage((p) => p - 1)}>
            ←
          </button>
          <span style={{ color: "var(--text-2)", alignSelf: "center" }}>
            {page + 1} / {pages}
          </span>
          <button
            className="btn btn-ghost"
            disabled={page >= pages - 1}
            onClick={() => setPage((p) => p + 1)}
          >
            →
          </button>
        </div>
      )}
    </div>
  );
}
