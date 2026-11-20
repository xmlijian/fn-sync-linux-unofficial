// 登录页 — 输入 NAS 地址（IP:5667），OAuth 授权，保存账号（官方 LoginForm 语义）
import { useEffect, useState } from "react";
import { commands, getConnectionType, ensurePort } from "../lib/commands";
import { t } from "../i18n";

interface Props {
  onConnected: () => void;
}

export default function LoginPage({ onConnected }: Props) {
  const [address, setAddress] = useState("");
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState("");
  const [devices, setDevices] = useState<Array<{ name: string; host: string; port: number; ips: string[] }>>([]);
  const [searching, setSearching] = useState(false);

  const discover = async () => {
    setSearching(true);
    try {
      const d = await commands.mdnsDiscoverDevices();
      setDevices(d);
    } catch {
      // mdns 不可用忽略
    } finally {
      setSearching(false);
    }
  };

  useEffect(() => {
    discover();
  }, []);

  const login = async () => {
    setBusy(true);
    setErr("");
    try {
      const trimmed = address.trim();
      if (!trimmed) throw new Error("empty address");
      if (!username.trim()) throw new Error("empty username");
      const connType = getConnectionType(trimmed);
      if (connType === "fnid") {
        throw new Error("FN ID 连接需要 FN Connect 中继，当前版本请使用 IP 连接");
      }
      const addr = ensurePort(trimmed);
      const accId = await commands.loginNas(addr, username.trim(), password);
      if (typeof accId === "number" && accId > 0) {
        onConnected();
      } else {
        throw new Error("login failed");
      }
    } catch (e) {
      const msg = String(e);
      setErr(msg.replace(/^.*Error\(("|')?(.*?)("|')?\)?.*$/, "$2") || msg);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="login-wrap">
      <div className="login-card">
        <div className="login-title">{t("login.connectToNas", "Connect to NAS")}</div>
        <div className="login-sub">
          {t("login.placeholder.address", "Enter an IP address, domain, or FN ID")} · 默认端口 5667
        </div>
        <input
          type="text"
          placeholder="192.168.9.214:5667"
          value={address}
          onChange={(e) => setAddress(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && !busy && login()}
        />
        <input
          type="text"
          placeholder={t("login.form.usernamePlaceholder", "Username")}
          value={username}
          onChange={(e) => setUsername(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && !busy && login()}
        />
        <input
          type="password"
          placeholder={t("login.form.passwordPlaceholder", "Password")}
          value={password}
          onChange={(e) => setPassword(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && !busy && login()}
        />
        <div className="login-err">{err}</div>
        <button className="btn-primary" disabled={busy} onClick={login}>
          {busy ? t("sync.status.connecting", "Connecting..") : t("login.connectToNas", "Connect")}
        </button>

        <div style={{ borderTop: "1px solid var(--border)", margin: "6px 0" }} />
        <div className="login-sub">
          {searching
            ? t("mdns.searching", "Searching")
            : devices.length > 0
              ? `${t("mdns.foundDevices", "Found")} ${devices.length}`
              : t("mdns.useIpInstead", "Can't find the device? Use IP connection")}
        </div>
        {devices.map((d) => (
          <button
            key={d.name}
            className="btn btn-ghost"
            style={{ textAlign: "left" }}
            onClick={() => setAddress(`${d.ips[0] ?? d.host}:${d.port}`)}
          >
            {d.host} · {d.ips[0] ?? ""}:{d.port}
          </button>
        ))}
      </div>
    </div>
  );
}
