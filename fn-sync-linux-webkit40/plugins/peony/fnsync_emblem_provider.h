#ifndef FNSYNC_EMBLEM_PROVIDER_H
#define FNSYNC_EMBLEM_PROVIDER_H

#include <QElapsedTimer>
#include <QHash>
#include <QMutex>
#include <QStringList>

#include "emblem-provider.h"

// 给 Peony 文件视图提供按需同步状态角标。
// 角标名与 deb 安装的 hicolor emblem 图标一一对应
// (/usr/share/icons/hicolor/scalable/emblems/emblem-fnsync-*.svg)。
class FnSyncEmblemProvider : public Peony::EmblemProvider
{
    Q_OBJECT
public:
    explicit FnSyncEmblemProvider(QObject *parent = nullptr);

    const QString emblemKey() override;
    QStringList getFileEmblemIcons(const QString &uri) override;

private Q_SLOTS:
    void onStatesChanged(const QStringList &paths);

private:
    // Peony 查询时传入的 uri 原样保存：状态变化后要用同一个 uri 通知刷新，
    // 否则中文/特殊字符路径（编码形式不同）匹配不上，图标只能等手动刷新。
    mutable QMutex m_uriMutex;
    QHash<QString, QString> m_uris;
    QElapsedTimer m_lastFullRefresh;
};

#endif // FNSYNC_EMBLEM_PROVIDER_H
