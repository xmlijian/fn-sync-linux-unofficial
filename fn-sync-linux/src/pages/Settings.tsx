// 设置页 — 语言 / 开机自启 / 全局限速 / 日志清理 / 版本信息（官方 Settings 页语义）
import { useEffect, useState } from "react";
import { commands, formatBytes } from "../lib/commands";
import { t, getLocale, setLocale, localeNames, LocaleKey } from "../i18n";

interface Props {
  onChanged: () => void;
}

export default function SettingsPage({ onChanged }: Props) {
  const [autostart, setAutostart] = useState(false);
  const [autoUpdate, setAutoUpdate] = useState(true);
  const [version, setVersion] = useState("");
  const [gitHash, setGitHash] = useState("");
  const [logDir, setLogDir] = useState("");
  const [locale, setLocaleState] = useState<LocaleKey>(getLocale());
  const [upLimit, setUpLimit] = useState(0);
  const [downLimit, setDownLimit] = useState(0);
  const [saveMsg, setSaveMsg] = useState("");

  useEffect(() => {
    (async () => {
      setAutostart(await commands.getAutostartState());
      const table = await commands.getAllSystemTable();
      setAutoUpdate(table.auto_update !== "0");
      const up = Number(table.global_upload_speed_limit ?? 0);
      const down = Number(table.global_download_speed_limit ?? 0);
      setUpLimit(up > 0 ? up : 0);
      setDownLimit(down > 0 ? down : 0);
      try {
        const { getVersion } = await import("@tauri-apps/api/app");
        setVersion(await getVersion());
      } catch {
        setVersion("0.2.3");
      }
      setGitHash(await commands.getGitHash());
      setLogDir(await commands.getAppLogDir());
    })();
  }, []);

  const toggleAutostart = async (v: boolean) => {
    setAutostart(v);
    await commands.setAutostartState(v);
  };

  const toggleAutoUpdate = async (v: boolean) => {
    setAutoUpdate(v);
    await commands.updateSystemTable("auto_update", v ? "1" : "0");
  };

  const changeLocale = (l: LocaleKey) => {
    setLocaleState(l);
    setLocale(l);
    void commands.updateSystemTable("language", l).then(() => {
      // 官方语义：语言切换重启应用生效
      commands.appRestart();
    });
  };

  const saveSpeedLimits = async () => {
    await commands.updateSystemTable("global_upload_speed_limit", String(upLimit));
    await commands.updateSystemTable("global_download_speed_limit", String(downLimit));
    setSaveMsg(t("status.settingSuccess", "Settings applied successfully"));
    setTimeout(() => setSaveMsg(""), 2000);
    onChanged();
  };

  return (
    <div className="page">
      <div className="settings-group">
        <div className="settings-row">
          <span className="settings-label">{t("settings.language", "Language")}</span>
          <select
            style={{ width: 160 }}
            value={locale}
            onChange={(e) => changeLocale(e.target.value as LocaleKey)}
          >
            {Object.entries(localeNames).map(([k, name]) => (
              <option key={k} value={k}>{name}</option>
            ))}
          </select>
        </div>
        <div className="settings-row">
          <span className="settings-label">{t("settings.autoStart", "Launch at startup")}</span>
          <label className="switch">
            <input
              type="checkbox"
              checked={autostart}
              onChange={(e) => toggleAutostart(e.target.checked)}
            />
            <span className="slider" />
          </label>
        </div>
        <div className="settings-row">
          <span className="settings-label">{t("settings.autoUpdate", "Automatically update")}</span>
          <label className="switch">
            <input
              type="checkbox"
              checked={autoUpdate}
              onChange={(e) => toggleAutoUpdate(e.target.checked)}
            />
            <span className="slider" />
          </label>
        </div>
      </div>

      <div className="settings-group">
        <div className="settings-row">
          <span className="settings-label">{t("settings.maxUploadSpeed", "Max upload speed")} (KB/s, 0=∞)</span>
          <input
            type="text"
            style={{ width: 140 }}
            value={upLimit}
            onChange={(e) => setUpLimit(Math.max(0, Number(e.target.value) || 0))}
          />
        </div>
        <div className="settings-row">
          <span className="settings-label">{t("settings.maxDownloadSpeed", "Max download speed")} (KB/s, 0=∞)</span>
          <input
            type="text"
            style={{ width: 140 }}
            value={downLimit}
            onChange={(e) => setDownLimit(Math.max(0, Number(e.target.value) || 0))}
          />
        </div>
        <div className="settings-row">
          <span className="settings-label">{saveMsg || `0 KB/s = ${formatBytes(0)}`}</span>
          <button className="btn-primary" onClick={saveSpeedLimits}>
            {t("action.create", "Save")}
          </button>
        </div>
      </div>

      <div className="settings-group">
        <div className="settings-row">
          <span className="settings-label">{t("settings.currentVersion", "Current Version")}</span>
          <span>{version} ({gitHash.slice(0, 8)})</span>
        </div>
        <div className="settings-row">
          <span className="settings-label">{t("settings.logCleanupRules", "Log directory")}</span>
          <span style={{ fontSize: 12, color: "var(--text-2)" }}>{logDir}</span>
        </div>
      </div>
    </div>
  );
}
