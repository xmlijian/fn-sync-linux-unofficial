// Tauri 命令封装 — 方法名与官方前端 commands$1 一致（子集）。
import { invoke } from "@tauri-apps/api/core";

export interface NasAccount {
  id: number;
  nas_id: string;
  uid: number;
  nas_name: string;
  address: string;
  username: string;
  conn_type: string;
  secret_data: string;
  long_token: string;
  token: string;
  failed_code: number;
  pub_key: string;
  secret: string;
  si: string;
  computer_name: string;
  fnos_version: string;
  cookie: string;
  created_at: number;
  updated_at: number;
}

export interface Task {
  id: number;
  nas_acc_id: number;
  name: string;
  address: string;
  conn_type: string;
  uuid: string;
  identifier: string;
  mode: "bisynchronous" | "uploadonly" | "downloadonly";
  nas_path: string;
  pc_path: string;
  status: number;
  is_init_sync: number;
  open_cloud: number;
  created_at: number;
  updated_at: number;
}

export interface FilterRule {
  ignore_dotfiles: number;
  enable_ignore_ext: number;
  ignore_extensions: string[];
  enable_size_lower: number;
  size_lower_bound: number | null;
  enable_size_upper: number;
  size_upper_bound: number | null;
}

export interface LogEntry {
  id: number;
  task_id: number;
  is_dir: number;
  name: string;
  path: string;
  sync_folder: string;
  action_code: number;
  status: number;
  failed_code: number;
  failed_text: string | null;
  payload: string | null;
  created_at: number;
}

// 官方 TaskStatus 枚举
export const TaskStatus = {
  INIT: 1,
  SYNCING: 2,
  PAUSED: 3,
  COMPLETED: 4,
  FAILED: 5,
  SEGMENT_COMPLETED: 6,
  ERROR: 7,
  IDLE: 8,
  ACCOUNT_ERROR: 9,
  FAILED_LOCAL_DIR: 10,
  FAILED_NAS_DIR: 11,
  VERSION_INCORRECT: 12,
  NAS_NO_ACL_SUPPORT: 14,
  NAS_NO_PERM: 15,
  NAS_FILE_NOT_EXISTS: 16,
  SECURITY_CODE_ERROR: 17,
} as const;

export const ActionCode = {
  Upload: 1,
  Download: 2,
  Delete: 3,
  Rename: 4,
  Exception: 5,
  MetaCreate: 6,
  Move: 7,
} as const;

export const commands = {
  // system / app
  getGitHash: () => invoke<string>("get_git_hash"),
  appQuit: () => invoke<void>("app_quit"),
  appRestart: () => invoke<void>("app_restart"),
  getOwnPid: () => invoke<number>("get_own_pid"),
  isWebviewDebug: () => invoke<boolean>("is_webview_debug"),
  getTrafficQuery: () => invoke<{ tx: number; rx: number }>("get_traffic_query"),
  getAppLogDir: () => invoke<string>("get_app_log_dir"),
  getDeviceId: () => invoke<string>("get_device_id"),
  setAutostartState: (enabled: boolean) => invoke<void>("set_autostart_state", { enabled }),
  getAutostartState: () => invoke<boolean>("get_autostart_state"),

  // system_table
  getAllSystemTable: () => invoke<Record<string, string>>("get_all_system_table"),
  updateSystemTable: (key: string, value: string) =>
    invoke<void>("update_system_table", { key, value }),

  // nas accounts
  addNasAccount: (account: Partial<NasAccount> & { nas_id: string; address: string; username: string }) =>
    invoke<number>("add_nas_account", { account }),
  getAllNasAccounts: () => invoke<NasAccount[]>("get_all_nas_accounts"),
  getNasAccountById: (id: number) => invoke<NasAccount | null>("get_nas_account_by_id", { id }),
  getNasAccountByNasId: (nasId: string) =>
    invoke<NasAccount | null>("get_nas_account_by_nas_id", { nasId }),
  getNasAccountByAddressAndUsername: (address: string, username: string) =>
    invoke<NasAccount | null>("get_nas_account_by_address_and_username", { address, username }),
  deleteNasAccount: (id: number) => invoke<void>("delete_nas_account", { id }),
  updateNasAccountByAuth: (
    id: number,
    user: string,
    token: string,
    longToken: string,
    secret: string,
    cookie: string
  ) =>
    invoke<void>("update_nas_account_by_auth", {
      id,
      username: user,
      token,
      longToken,
      secret,
      cookie,
    }),
  updateNasConnTypeById: (id: number, connType: string, address: string) =>
    invoke<void>("update_nas_conn_type_by_id", { id, connType, address }),

  // connect
  checkTrimAddress: (connType: string, address: string) =>
    invoke<void>("check_trim_address", { connType, address }),
  queryNasOsEdition: (address: string) =>
    invoke<Record<string, unknown>>("query_nas_os_edition", { address }),
  openOauthWindow: (url: string, connType: string, accountId?: number) =>
    invoke<void>("open_oauth_window", { url, connType, accountId }),
  putAuthResult: (data: Record<string, unknown>) =>
    invoke<void>("put_auth_result", { data }),
  connectNasByAuth: (config: Record<string, unknown>) =>
    invoke<number>("connect_nas_by_auth", { config }),
  loginNas: (address: string, username: string, password: string) =>
    invoke<number>("login_nas", { address, username, password }),
  createNasClient: (connType: string, address: string) =>
    invoke<number>("create_nas_client", { connType, address }),
  deleteNasClient: (connectionId: number) =>
    invoke<void>("delete_nas_client", { connectionId }),
  sendMessageFromConnectionId: (connectionId: number, message: Record<string, unknown>) =>
    invoke<Record<string, unknown>>("send_message_from_connection_id", {
      connectionId,
      message,
    }),
  getAddrType: (connType: string, address: string) =>
    invoke<string>("get_addr_type", { connType, address }),
  mdnsDiscoverDevices: () =>
    invoke<Array<{ name: string; host: string; port: number; ips: string[] }>>(
      "mdns_discover_devices"
    ),

  // tasks
  addTask: (task: Record<string, unknown>) => invoke<number>("add_task", { task }),
  getAllTasks: () => invoke<Task[]>("get_all_tasks"),
  getTaskById: (id: number) => invoke<Task | null>("get_task_by_id", { id }),
  getTaskCount: (taskId: number) => invoke<number>("get_task_count", { taskId }),
  getNextTaskId: () => invoke<number>("get_next_task_id"),
  updateTask: (id: number, updateData: Record<string, unknown>) =>
    invoke<void>("update_task", { id, updateData }),
  updateTaskStatus: (id: number, status: number) =>
    invoke<void>("update_task_status", { id, status }),
  deleteTask: (id: number) => invoke<void>("delete_task", { id }),
  pauseTask: (taskId: number) => invoke<void>("pause_task", { taskId }),
  resumeTask: (taskId: number) => invoke<void>("resume_task", { taskId }),
  reloadTask: (taskId: number) => invoke<void>("reload_task", { taskId }),
  getTaskStatusSummary: (taskId: number) =>
    invoke<{ status: number; running: boolean; mode: string }>("get_task_status_summary", { taskId }),
  getGlobalTasksStatus: () =>
    invoke<{ task_count: number; running_count: number }>("get_global_tasks_status"),
  getFilterRuleByTaskId: (taskId: number) =>
    invoke<FilterRule | null>("get_filter_rule_by_task_id", { taskId }),
  saveFolderFilterPaths: (taskId: number, folderPaths: string[]) =>
    invoke<void>("save_folder_filter_paths", { taskId, folderPaths }),
  getFolderFilterPaths: (taskId: number) =>
    invoke<string[]>("get_folder_filter_paths", { taskId }),
  validateAppName: (name: string) => invoke<void>("validate_app_name", { name }),
  validateTaskNasDirExists: (taskId: number, nasPath: string) =>
    invoke<boolean>("validate_task_nas_dir_exists", { taskId, nasPath }),

  // logs
  queryLogs: (params: Record<string, unknown>) =>
    invoke<{ data: LogEntry[]; total: number }>("query_logs", { params }),
  getLogsBySize: (size: number) => invoke<LogEntry[]>("get_logs_by_size", { size }),
  clearAllLogs: () => invoke<void>("clear_all_logs"),
  getExtendedLogs: () => invoke<LogEntry[]>("get_extended_logs"),
  getCompletedItems: (taskId: number) => invoke<LogEntry[]>("get_completed_items", { taskId }),
};

// 地址类型识别（与官方 getConnectionType 一致）
const ipv4 = /^(\d{1,3}\.){3}\d{1,3}(:\d{1,5})?$/;
const domain = /^[a-zA-Z0-9][-a-zA-Z0-9]{0,62}(\.[a-zA-Z0-9][-a-zA-Z0-9]{0,62})+(:\d{1,5})?$/;

export function getConnectionType(address: string): "ip" | "host" | "fnid" {
  if (ipv4.test(address)) return "ip";
  if (domain.test(address)) return "host";
  return "fnid";
}

export function ensurePort(address: string): string {
  if (!address.includes(":")) return `${address}:5667`;
  return address;
}

export function formatSpeed(bytes: number): string {
  if (bytes <= 0) return "0 B/s";
  const units = ["B/s", "KB/s", "MB/s", "GB/s"];
  let v = bytes;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(1)} ${units[i]}`;
}

export function formatBytes(bytes: number): string {
  if (bytes <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let v = bytes;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(v >= 100 || i === 0 ? 0 : 1)} ${units[i]}`;
}

export function formatTime(ms: number): string {
  return new Date(ms).toLocaleString();
}
