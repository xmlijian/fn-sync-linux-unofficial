import { useEffect, useState } from "react";
import { t, getLocale, setLocale, localeNames, LocaleKey } from "./i18n";
import { commands, TaskStatus } from "./lib/commands";
import TasksPage from "./pages/Tasks";
import LoginPage from "./pages/Login";
import SettingsPage from "./pages/Settings";
import LogsPage from "./pages/Logs";
import { formatSpeed } from "./lib/commands";

type Route = "tasks" | "logs" | "settings";

const statusLabel = (s: number): string => {
  switch (s) {
    case TaskStatus.INIT: return t("sync.status.connecting", "Connecting..");
    case TaskStatus.SYNCING: return t("sync.status.syncing", "Syncing");
    case TaskStatus.PAUSED: return t("sync.status.paused", "Paused");
    case TaskStatus.COMPLETED: return t("sync.status.completed", "Sync Completed");
    case TaskStatus.FAILED: return t("sync.status.error", "Status Error");
    case TaskStatus.SEGMENT_COMPLETED: return t("sync.status.partialSuccess", "Partial Success");
    case TaskStatus.ERROR: return t("sync.status.failed", "Sync Failed");
    case TaskStatus.ACCOUNT_ERROR: return t("sync.status.accountError", "Account error");
    case TaskStatus.FAILED_LOCAL_DIR:
    case TaskStatus.FAILED_NAS_DIR: return t("task.filePathOutsideSyncDir", "Path error");
    case TaskStatus.VERSION_INCORRECT: return t("error.clientVersionCannotConnect", "Version incorrect");
    case TaskStatus.SECURITY_CODE_ERROR: return t("sync.status.securityCodeError", "Security code error");
    default: return t("status.error", "Error");
  }
};

const modeLabel = (m: string): string => {
  switch (m) {
    case "bisynchronous": return t("sync.mode.twoWay", "Two-Way Sync");
    case "downloadonly": return t("sync.mode.downloadOnly", "Download Only");
    case "uploadonly": return t("sync.mode.uploadOnly", "Upload Only");
    default: return m;
  }
};

export default function App() {
  const [route, setRoute] = useState<Route>("tasks");
  const [accounts, setAccounts] = useState<Awaited<ReturnType<typeof commands.getAllNasAccounts>>>([]);
  const [tasks, setTasks] = useState<Awaited<ReturnType<typeof commands.getAllTasks>>>([]);
  const [traffic, setTraffic] = useState({ tx: 0, rx: 0 });
  const [localeTick, setLocaleTick] = useState(0);

  const refresh = async () => {
    try {
      const [a, ts] = await Promise.all([commands.getAllNasAccounts(), commands.getAllTasks()]);
      setAccounts(a);
      setTasks(ts);
    } catch {
      // app state 可能尚未就绪
    }
  };

  useEffect(() => {
    refresh();
    const timer = setInterval(refresh, 2000);
    const trafficTimer = setInterval(async () => {
      try {
        setTraffic(await commands.getTrafficQuery());
      } catch {}
    }, 1500);
    return () => {
      clearInterval(timer);
      clearInterval(trafficTimer);
    };
  }, []);

  const changeLocale = (l: LocaleKey) => {
    setLocale(l);
    setLocaleTick((x) => x + 1);
  };

  if (accounts.length === 0) {
    return <LoginPage key={localeTick} onConnected={refresh} />;
  }

  return (
    <div className="app" key={localeTick}>
      <aside className="sidebar">
        <div className="brand">
          <span className="brand-icon">⇄</span>
          <span>{t("nav.syncTasks", "Sync Tasks")}</span>
        </div>
        <nav>
          <button
            className={route === "tasks" ? "active" : ""}
            onClick={() => setRoute("tasks")}
          >
            {t("nav.syncTasks", "Sync Tasks")}
          </button>
          <button
            className={route === "logs" ? "active" : ""}
            onClick={() => setRoute("logs")}
          >
            {t("nav.syncLogs", "Sync Logs")}
          </button>
          <button
            className={route === "settings" ? "active" : ""}
            onClick={() => setRoute("settings")}
          >
            {t("nav.settings", "Settings")}
          </button>
        </nav>
        <div className="sidebar-footer">
          <div className="traffic">
            <div>↓ {formatSpeed(traffic.rx)}</div>
            <div>↑ {formatSpeed(traffic.tx)}</div>
          </div>
          <select
            value={getLocale()}
            onChange={(e) => changeLocale(e.target.value as LocaleKey)}
            aria-label={t("settings.language", "Language")}
          >
            {Object.entries(localeNames).map(([k, name]) => (
              <option key={k} value={k}>{name}</option>
            ))}
          </select>
        </div>
      </aside>
      <main className="content">
        <header className="topbar" data-tauri-drag-region>
          <h1>
            {route === "tasks"
              ? t("nav.syncTasks", "Sync Tasks")
              : route === "logs"
                ? t("nav.syncLogs", "Sync Logs")
                : t("nav.settings", "Settings")}
          </h1>
          <span className="nas-badge">
            {accounts.map((a) => a.nas_name).join(" · ")}
          </span>
        </header>
        {route === "tasks" && <TasksPage tasks={tasks} accounts={accounts} onChanged={refresh} statusLabel={statusLabel} modeLabel={modeLabel} />}
        {route === "logs" && <LogsPage />}
        {route === "settings" && <SettingsPage onChanged={refresh} />}
      </main>
    </div>
  );
}
