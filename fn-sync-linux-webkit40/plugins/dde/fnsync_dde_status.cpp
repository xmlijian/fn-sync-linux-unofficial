#include "fnsync_dde_status.h"

#include <nlohmann/json.hpp>

#include <dirent.h>
#include <fcntl.h>
#include <poll.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>

#include <cerrno>
#include <chrono>
#include <cstdlib>
#include <cstring>
#include <fstream>
#include <thread>

namespace {

constexpr int kReadTimeoutMs = 1500;
constexpr long long kStateGraceMs = 30000;
constexpr int kPollIntervalMs = 1500;

long long nowMs()
{
    using namespace std::chrono;
    return duration_cast<milliseconds>(steady_clock::now().time_since_epoch()).count();
}

bool fileExists(const std::string &path)
{
    struct stat st;
    return ::stat(path.c_str(), &st) == 0;
}

bool isDirectory(const std::string &path)
{
    struct stat st;
    return ::stat(path.c_str(), &st) == 0 && S_ISDIR(st.st_mode);
}

// 纯 POSIX AF_UNIX 查询：不依赖 Qt 事件循环，可在任意线程安全调用。
std::string queryStatus(const std::string &socketPath)
{
    const int fd = ::socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (fd < 0)
        return {};

    sockaddr_un address = {};
    address.sun_family = AF_UNIX;
    if (socketPath.size() + 1 > sizeof(address.sun_path)) {
        ::close(fd);
        return {};
    }
    std::memcpy(address.sun_path, socketPath.c_str(), socketPath.size() + 1);
    if (::connect(fd, reinterpret_cast<sockaddr *>(&address), sizeof(address)) != 0) {
        ::close(fd);
        return {};
    }

    const char request[] = "{\"action\":\"status\"}\n";
    if (::send(fd, request, sizeof(request) - 1, MSG_NOSIGNAL) != ssize_t(sizeof(request) - 1)) {
        ::close(fd);
        return {};
    }

    std::string data;
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::milliseconds(kReadTimeoutMs);
    while (std::chrono::steady_clock::now() < deadline) {
        if (!data.empty() && data.back() == '\n')
            break;
        struct pollfd watch = {fd, POLLIN, 0};
        if (::poll(&watch, 1, 200) <= 0)
            continue;
        char buffer[65536];
        const ssize_t count = ::read(fd, buffer, sizeof(buffer));
        if (count <= 0)
            break;
        data.append(buffer, size_t(count));
    }
    ::close(fd);
    return data;
}

} // namespace

void fnsyncDebug(const std::string &message)
{
    static const bool enabled = std::getenv("FNSYNC_DDE_DEBUG") != nullptr;
    if (!enabled)
        return;
    std::ofstream out("/tmp/fnsync-dde.log", std::ios::app);
    if (!out)
        return;
    const auto now = std::chrono::system_clock::now();
    const auto ms = std::chrono::duration_cast<std::chrono::milliseconds>(now.time_since_epoch()).count();
    out << ms << ' ' << message << '\n';
}

FnSyncStatusClient &FnSyncStatusClient::instance()
{
    static FnSyncStatusClient client;
    return client;
}

std::string FnSyncStatusClient::ondemandRoot()
{
    const char *home = std::getenv("HOME");
    return std::string(home ? home : "") + "/.local/share/fn-sync-client/ondemand";
}

std::string FnSyncStatusClient::normalizePath(const std::string &path)
{
    std::string result = path;
    while (result.size() > 1 && result.back() == '/')
        result.pop_back();
    return result;
}

void FnSyncStatusClient::startPolling()
{
    if (m_polling.exchange(true))
        return;
    std::thread([this]() {
        for (;;) {
            refresh(true);
            std::this_thread::sleep_for(std::chrono::milliseconds(kPollIntervalMs));
        }
    }).detach();
}

void FnSyncStatusClient::refreshNow()
{
    refresh(false);
}

bool FnSyncStatusClient::lookup(const std::string &absolutePath, FnSyncState *state) const
{
    const std::string path = normalizePath(absolutePath);
    std::lock_guard<std::mutex> locker(m_mutex);
    const auto it = m_cache.find(path);
    if (it == m_cache.end())
        return false;
    if (state)
        *state = it->second;
    return true;
}

bool FnSyncStatusClient::contains(const std::string &absolutePath) const
{
    const std::string path = normalizePath(absolutePath);
    std::lock_guard<std::mutex> locker(m_mutex);
    if (m_cache.find(path) != m_cache.end())
        return true;
    for (const std::string &mount : m_mountpoints) {
        if (path == mount || path.compare(0, mount.size() + 1, mount + "/") == 0)
            return true;
    }
    return false;
}

void FnSyncStatusClient::setStatesChanged(std::function<void(const std::vector<std::string> &)> callback)
{
    std::lock_guard<std::mutex> locker(m_mutex);
    m_callback = std::move(callback);
}

void FnSyncStatusClient::refresh(bool emitChanges)
{
    if (m_refreshing.exchange(true))
        return;

    std::map<std::string, FnSyncState> snapshot;
    std::vector<std::string> mountpoints;

    const std::string root = ondemandRoot();
    DIR *dir = ::opendir(root.c_str());
    if (dir) {
        std::vector<std::string> tasks;
        while (dirent *entry = ::readdir(dir)) {
            const std::string name = entry->d_name;
            if (name == "." || name == "..")
                continue;
            if (isDirectory(root + "/" + name))
                tasks.push_back(name);
        }
        ::closedir(dir);

        for (const std::string &task : tasks) {
            const std::string socketPath = root + "/" + task + "/control.sock";
            if (!fileExists(socketPath))
                continue;
            const std::string data = queryStatus(socketPath);
            const size_t newline = data.find('\n');
            if (newline == std::string::npos)
                continue;
            nlohmann::json reply = nlohmann::json::parse(data.substr(0, newline), nullptr, false);
            if (reply.is_discarded() || !reply.is_object())
                continue;
            if (!reply.value("ok", false))
                continue;
            const nlohmann::json result = reply.value("result", nlohmann::json::object());
            const std::string mountpoint = normalizePath(result.value("mountpoint", std::string()));
            if (mountpoint.empty())
                continue;
            mountpoints.push_back(mountpoint);
            // 挂载根目录本身也登记为 directory：右键这个文件夹时展示文件夹级动作。
            snapshot[mountpoint] = FnSyncState{"directory", false};

            for (const auto &file : result.value("files", nlohmann::json::array())) {
                if (!file.is_object())
                    continue;
                const std::string rel = file.value("path", std::string());
                const std::string absolute = rel.empty() ? mountpoint : mountpoint + "/" + rel;
                FnSyncState state;
                state.state = file.value("state", std::string());
                state.pinned = file.value("pinned", false);
                snapshot[normalizePath(absolute)] = state;
            }
        }
    }

    fnsyncDebug("refresh files=" + std::to_string(snapshot.size()));
    {
        std::lock_guard<std::mutex> locker(m_mutex);
        m_mountpoints = mountpoints;
    }
    m_refreshing.store(false);
    applySnapshot(snapshot, emitChanges);
}

void FnSyncStatusClient::applySnapshot(const std::map<std::string, FnSyncState> &snapshot, bool emitChanges)
{
    std::vector<std::string> changed;
    std::function<void(const std::vector<std::string> &)> callback;
    {
        std::lock_guard<std::mutex> locker(m_mutex);
        const long long now = nowMs();

        // 快照短暂缺文件时保留旧状态，避免角标整屏闪烁。
        std::map<std::string, FnSyncState> merged = snapshot;
        for (const auto &item : m_cache) {
            if (merged.count(item.first))
                continue;
            if (now - m_seen[item.first] <= kStateGraceMs)
                merged[item.first] = item.second;
        }
        for (const auto &item : merged)
            m_seen[item.first] = now;

        if (emitChanges) {
            std::vector<std::string> keys;
            keys.reserve(merged.size() + m_cache.size());
            for (const auto &item : merged)
                keys.push_back(item.first);
            for (const auto &item : m_cache)
                keys.push_back(item.first);
            for (const std::string &key : keys) {
                const auto before = m_cache.find(key);
                const auto after = merged.find(key);
                const std::string beforeState = before == m_cache.end() ? std::string() : before->second.state;
                const std::string afterState = after == merged.end() ? std::string() : after->second.state;
                if (beforeState != afterState)
                    changed.push_back(key);
            }
        }

        m_cache = merged;
        for (auto it = m_seen.begin(); it != m_seen.end();) {
            if (now - it->second > 5 * 60 * 1000)
                it = m_seen.erase(it);
            else
                ++it;
        }
        callback = m_callback;
    }

    if (emitChanges && !changed.empty() && callback)
        callback(changed);
}
