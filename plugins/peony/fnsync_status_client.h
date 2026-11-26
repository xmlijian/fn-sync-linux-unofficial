#ifndef FNSYNC_STATUS_CLIENT_H
#define FNSYNC_STATUS_CLIENT_H

#include <QHash>
#include <QMutex>
#include <QObject>
#include <QString>
#include <QStringList>

// 调试日志（排查文件管理器插件加载/状态匹配用），输出到 /tmp/fnsync-peony.log。
void fnsyncDebug(const QString &message);

struct FnSyncState
{
    QString state;
    bool pinned = false;
};

// 按需同步守护进程状态缓存。
//
// 守护进程为每个挂载点提供一个控制 socket
// (~/.local/share/fn-sync-client/ondemand/<task>/control.sock)，发送
// {"action":"status"} 返回各文件的底层状态（placeholder/cached/download/...）。
// 角标插件在后台线程定期拉取；右键菜单插件在弹出前同步刷新一次。
class FnSyncStatusClient : public QObject
{
    Q_OBJECT
public:
    static FnSyncStatusClient *instance();

    // 启动后台轮询（幂等）。
    void startPolling();

    // 立即同步刷新一次，带局部超时，不会长时间阻塞文件管理器。
    void refreshNow();

    // 查询绝对路径的状态；不属于任何按需挂载点时返回 false。
    bool lookup(const QString &absolutePath, FnSyncState *state) const;

    // 该路径是否位于某个按需挂载点内（未列出的子路径也算）。
    bool contains(const QString &absolutePath) const;

Q_SIGNALS:
    // 图标展示相关状态发生变化的绝对路径（供 emblem 插件刷新角标）。
    void statesChanged(const QStringList &changedPaths);

private:
    explicit FnSyncStatusClient(QObject *parent = nullptr);

    void refresh(bool emitChanges);
    void applySnapshot(const QHash<QString, FnSyncState> &snapshot, bool emitChanges);

    static QString ondemandRoot();

    mutable QMutex m_mutex;
    QHash<QString, FnSyncState> m_cache;
    QHash<QString, qint64> m_seen;
    QStringList m_mountpoints;
    bool m_polling = false;
    bool m_refreshing = false;
};

#endif // FNSYNC_STATUS_CLIENT_H
