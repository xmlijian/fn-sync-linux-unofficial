#ifndef FNSYNC_DDE_STATUS_H
#define FNSYNC_DDE_STATUS_H

#include <atomic>
#include <functional>
#include <map>
#include <mutex>
#include <string>
#include <vector>

// 与 Peony 插件共用同一份守护进程状态协议，但这里不依赖 Qt：
// DDE 的 dfm-extension 接口是纯 C++（std），文件管理器进程里也没有
// 可用的 QObject 事件循环给我们，所以轮询用 std::thread + POSIX socket。

void fnsyncDebug(const std::string &message);

struct FnSyncState
{
    std::string state;
    bool pinned = false;
};

class FnSyncStatusClient
{
public:
    static FnSyncStatusClient &instance();

    // 启动后台轮询（幂等）。
    void startPolling();

    // 立即同步刷新一次（菜单弹出前用）。
    void refreshNow();

    // 查询绝对路径状态；不在任何按需挂载点内返回 false。
    bool lookup(const std::string &absolutePath, FnSyncState *state) const;

    // 路径是否位于某个按需挂载点内（未列出的子路径也算）。
    bool contains(const std::string &absolutePath) const;

    // 状态变化回调（在轮询线程里调用，实现方需自行处理线程安全）。
    void setStatesChanged(std::function<void(const std::vector<std::string> &)> callback);

private:
    FnSyncStatusClient() = default;

    void refresh(bool emitChanges);
    void applySnapshot(const std::map<std::string, FnSyncState> &snapshot, bool emitChanges);
    static std::string ondemandRoot();
    static std::string normalizePath(const std::string &path);

    mutable std::mutex m_mutex;
    std::map<std::string, FnSyncState> m_cache;
    std::map<std::string, long long> m_seen;
    std::vector<std::string> m_mountpoints;
    std::function<void(const std::vector<std::string> &)> m_callback;
    std::atomic<bool> m_polling{false};
    std::atomic<bool> m_refreshing{false};
};

#endif // FNSYNC_DDE_STATUS_H
