#include "fnsync_status_client.h"

#include <QDateTime>
#include <QDir>
#include <QElapsedTimer>
#include <QFileInfo>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QFile>
#include <QSet>

#include <poll.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#include <cerrno>
#include <chrono>
#include <cstring>
#include <thread>

namespace {

// 单次 status 请求的最长等待：守护进程在本地，正常几十毫秒返回。
constexpr int kReadTimeoutMs = 1500;
// 状态快照短暂缺文件时保留旧值的时长（守护进程重启、目录尚未展开）。
constexpr qint64 kStateGraceMs = 30000;
// 后台轮询间隔。
constexpr int kPollIntervalMs = 1500;

qint64 nowMs()
{
    return QDateTime::currentMSecsSinceEpoch();
}

QString normalizePath(const QString &path)
{
    QString result = path;
    while (result.size() > 1 && result.endsWith(QLatin1Char('/')))
        result.chop(1);
    return result;
}

// 用纯 POSIX AF_UNIX socket 查询守护进程状态。
//
// 这里刻意不用 QLocalSocket：本函数在裸 std::thread 里执行，Qt 的 socket
// 依赖线程事件分发器，在守护进程退出/连接被拒时会在 QMutex::lock 里崩溃
// （134 上抓到过 SIGSEGV：connectToServer -> setSocketDescriptor）。
QByteArray queryStatus(const QString &socketPath)
{
    const int fd = ::socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (fd < 0)
        return {};

    const QByteArray encoded = QFile::encodeName(socketPath);
    sockaddr_un address = {};
    address.sun_family = AF_UNIX;
    if (encoded.size() + 1 > int(sizeof(address.sun_path))) {
        ::close(fd);
        return {};
    }
    std::memcpy(address.sun_path, encoded.constData(), size_t(encoded.size()) + 1);

    if (::connect(fd, reinterpret_cast<sockaddr *>(&address), sizeof(address)) != 0) {
        ::close(fd);
        return {};
    }

    const char request[] = "{\"action\":\"status\"}\n";
    ssize_t written = ::send(fd, request, sizeof(request) - 1, MSG_NOSIGNAL);
    if (written != ssize_t(sizeof(request) - 1)) {
        ::close(fd);
        return {};
    }

    QByteArray data;
    QElapsedTimer timer;
    timer.start();
    while (!data.endsWith('\n') && timer.elapsed() < kReadTimeoutMs) {
        struct pollfd watch = {fd, POLLIN, 0};
        const int ready = ::poll(&watch, 1, 200);
        if (ready <= 0)
            continue;
        char buffer[65536];
        const ssize_t count = ::read(fd, buffer, sizeof(buffer));
        if (count <= 0)
            break;
        data.append(buffer, int(count));
    }

    ::close(fd);
    return data;
}

} // namespace

void fnsyncDebug(const QString &message)
{
    // 现场排查时设置 FNSYNC_PEONY_DEBUG=1 再启动文件管理器，日志写 /tmp/fnsync-peony.log。
    static const bool enabled = qEnvironmentVariableIsSet("FNSYNC_PEONY_DEBUG");
    if (!enabled)
        return;
    QFile file(QStringLiteral("/tmp/fnsync-peony.log"));
    if (!file.open(QIODevice::Append | QIODevice::Text))
        return;
    file.write((QDateTime::currentDateTime().toString(Qt::ISODateWithMs)
                + QStringLiteral(" ") + message + QStringLiteral("\n")).toUtf8());
}


FnSyncStatusClient *FnSyncStatusClient::instance()
{
    static FnSyncStatusClient *client = new FnSyncStatusClient();
    return client;
}

FnSyncStatusClient::FnSyncStatusClient(QObject *parent)
    : QObject(parent)
{
}

QString FnSyncStatusClient::ondemandRoot()
{
    return QDir::homePath() + QStringLiteral("/.local/share/fn-sync-client/ondemand");
}

void FnSyncStatusClient::startPolling()
{
    if (m_polling)
        return;
    m_polling = true;
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

bool FnSyncStatusClient::lookup(const QString &absolutePath, FnSyncState *state) const
{
    const QString path = normalizePath(absolutePath);
    QMutexLocker locker(&m_mutex);
    auto it = m_cache.constFind(path);
    if (it == m_cache.constEnd())
        return false;
    if (state)
        *state = it.value();
    return true;
}

bool FnSyncStatusClient::contains(const QString &absolutePath) const
{
    FnSyncState ignored;
    if (lookup(absolutePath, &ignored))
        return true;
    const QString path = normalizePath(absolutePath);
    QMutexLocker locker(&m_mutex);
    for (const QString &mount : m_mountpoints) {
        if (path == mount || path.startsWith(mount + QLatin1Char('/')))
            return true;
    }
    return false;
}

void FnSyncStatusClient::refresh(bool emitChanges)
{
    {
        QMutexLocker locker(&m_mutex);
        if (m_refreshing)
            return;
        m_refreshing = true;
    }

    QHash<QString, FnSyncState> snapshot;
    QStringList mountpoints;

    const QDir root(ondemandRoot());
    const QStringList tasks = root.entryList(QDir::Dirs | QDir::NoDotAndDotDot);
    for (const QString &task : tasks) {
        const QString socketPath = root.filePath(task + QStringLiteral("/control.sock"));
        if (!QFileInfo::exists(socketPath))
            continue;

        const QByteArray data = queryStatus(socketPath);
        const int newline = data.indexOf('\n');
        if (newline < 0)
            continue;
        const QJsonDocument document = QJsonDocument::fromJson(data.left(newline));
        if (!document.isObject())
            continue;
        const QJsonObject reply = document.object();
        if (!reply.value(QStringLiteral("ok")).toBool())
            continue;
        const QJsonObject result = reply.value(QStringLiteral("result")).toObject();
        const QString mountpoint = normalizePath(result.value(QStringLiteral("mountpoint")).toString());
        if (mountpoint.isEmpty())
            continue;
        mountpoints.append(mountpoint);
        // 挂载根目录本身也要有状态：文件管理器里右键这个文件夹时
        // （例如在上级目录选中按需同步目录）需要展示文件夹级动作。
        snapshot.insert(mountpoint, FnSyncState{QStringLiteral("directory"), false});

        const QJsonArray files = result.value(QStringLiteral("files")).toArray();
        for (const QJsonValue &value : files) {
            const QJsonObject file = value.toObject();
            const QString rel = file.value(QStringLiteral("path")).toString();
            const QString absolute = rel.isEmpty()
                ? mountpoint
                : mountpoint + QLatin1Char('/') + rel;
            FnSyncState state;
            state.state = file.value(QStringLiteral("state")).toString();
            state.pinned = file.value(QStringLiteral("pinned")).toBool();
            snapshot.insert(normalizePath(absolute), state);
        }
    }

    fnsyncDebug(QStringLiteral("refresh sockets=%1 files=%2").arg(tasks.size()).arg(snapshot.size()));
    {
        QMutexLocker locker(&m_mutex);
        m_refreshing = false;
        m_mountpoints = mountpoints;
    }
    applySnapshot(snapshot, emitChanges);
}

void FnSyncStatusClient::applySnapshot(const QHash<QString, FnSyncState> &snapshot, bool emitChanges)
{
    QStringList changed;
    {
        QMutexLocker locker(&m_mutex);
        const qint64 now = nowMs();

        // 快照暂时缺文件时保留旧状态，避免角标整屏闪一下。
        QHash<QString, FnSyncState> merged = snapshot;
        for (auto it = m_cache.constBegin(); it != m_cache.constEnd(); ++it) {
            if (merged.contains(it.key()))
                continue;
            if (now - m_seen.value(it.key(), 0) <= kStateGraceMs)
                merged.insert(it.key(), it.value());
        }

        for (auto it = merged.constBegin(); it != merged.constEnd(); ++it)
            m_seen.insert(it.key(), now);

        if (emitChanges) {
            // 不用 QSet 的迭代器区间构造：Qt 5.12（140 的文件管理器）没有。
            QSet<QString> keys;
            for (auto it = merged.constBegin(); it != merged.constEnd(); ++it)
                keys.insert(it.key());
            for (auto it = m_cache.constBegin(); it != m_cache.constEnd(); ++it)
                keys.insert(it.key());
            for (const QString &key : keys) {
                const QString before = m_cache.value(key).state;
                const QString after = merged.value(key).state;
                if (before != after)
                    changed.append(key);
            }
        }

        m_cache = merged;
        // 清理长时间未见到的条目（卸载/删除任务后）。
        for (auto it = m_seen.begin(); it != m_seen.end();) {
            if (now - it.value() > 5 * 60 * 1000)
                it = m_seen.erase(it);
            else
                ++it;
        }
    }

    if (emitChanges && !changed.isEmpty())
        emit statesChanged(changed);
}
