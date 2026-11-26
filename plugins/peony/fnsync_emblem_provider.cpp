#include "fnsync_emblem_provider.h"

#include <QMutexLocker>
#include <QFile>
#include <QUrl>

#include <fcntl.h>
#include <sys/stat.h>

#include "fnsync_status_client.h"

namespace {

// 与 Nautilus 扩展使用同一套图标名：
//   placeholder  → 在线（灰色空心云）
//   cached       → 已下载（绿色云+勾）
//   download     → 下载中
//   queued       → 排队中（复用下载中）
//   upload       → 上传中
//   releasing    → 释放中（复用在线）
//   dirty        → 本地已修改（橙色铅笔）
QString emblemForState(const QString &state)
{
    if (state == QLatin1String("placeholder"))
        return QStringLiteral("emblem-fnsync-online");
    if (state == QLatin1String("cached"))
        return QStringLiteral("emblem-fnsync-local");
    if (state == QLatin1String("download") || state == QLatin1String("queued"))
        return QStringLiteral("emblem-fnsync-downloading");
    if (state == QLatin1String("upload"))
        return QStringLiteral("emblem-fnsync-uploading");
    if (state == QLatin1String("releasing"))
        return QStringLiteral("emblem-fnsync-online");
    if (state == QLatin1String("dirty"))
        return QStringLiteral("emblem-fnsync-modified");
    return QString();
}

// 用「保持 mtime 不变」的 utime 触发一次真实的属性变更事件：
// Peony 收到 IN_ATTRIB 后会重新查询角标。只 invalidate 内核缓存不会产生
// inotify 事件，所以之前图标必须手动刷新才更新。
void touchAttributes(const QString &path)
{
    struct stat st;
    const QByteArray local = QFile::encodeName(path);
    if (::stat(local.constData(), &st) != 0)
        return;
    struct timespec times[2];
    times[0] = st.st_atim;
    times[1] = st.st_mtim;
    ::utimensat(AT_FDCWD, local.constData(), times, 0);
}

QString localPathOf(const QString &uri)
{
    if (uri.startsWith(QLatin1String("file://")))
        return QUrl(uri).toLocalFile();
    return uri;
}

} // namespace

FnSyncEmblemProvider::FnSyncEmblemProvider(QObject *parent)
    : Peony::EmblemProvider(parent)
{
    FnSyncStatusClient *client = FnSyncStatusClient::instance();
    connect(client, &FnSyncStatusClient::statesChanged,
            this, &FnSyncEmblemProvider::onStatesChanged,
            Qt::QueuedConnection);
    client->startPolling();
}

const QString FnSyncEmblemProvider::emblemKey()
{
    return QStringLiteral("fnsync-status");
}

QStringList FnSyncEmblemProvider::getFileEmblemIcons(const QString &uri)
{
    const QString path = localPathOf(uri);
    if (path.isEmpty())
        return {};

    FnSyncState state;
    if (!FnSyncStatusClient::instance()->lookup(path, &state))
        return {};
    {
        QMutexLocker locker(&m_uriMutex);
        m_uris.insert(path, uri);
    }

    const QString emblem = emblemForState(state.state);
    fnsyncDebug(QStringLiteral("emblem query %1 -> %2 (%3)").arg(path, emblem, state.state));
    if (emblem.isEmpty())
        return {};
    return {emblem};
}

void FnSyncEmblemProvider::onStatesChanged(const QStringList &paths)
{
    // 角标随下载/上传/缓存状态变化即时刷新，不等 Peony 下次重绘。
    // 优先用 Peony 自己传给我们的 uri（可能与 file://+路径 的编码不同）。
    fnsyncDebug(QStringLiteral("statesChanged %1").arg(paths.join(QLatin1Char(','))));
    QStringList uris;
    {
        QMutexLocker locker(&m_uriMutex);
        for (const QString &path : paths)
            uris.append(m_uris.value(path, QStringLiteral("file://") + path));
    }
    for (const QString &path : paths)
        touchAttributes(path);
    for (const QString &uri : uris)
        Q_EMIT requestUpdateFile(uri);

    // 单文件刷新在部分 Peony 视图（列表视图/已缓存行）不一定会重绘角标，
    // 再补一次全量刷新；状态变更是低频事件，做 2 秒节流避免频繁重绘。
    if (!m_lastFullRefresh.isValid() || m_lastFullRefresh.elapsed() > 2000) {
        m_lastFullRefresh.start();
        Q_EMIT requestUpdateAllFiles();
    }
}
