#ifndef FNSYNC_MENU_PLUGIN_H
#define FNSYNC_MENU_PLUGIN_H

#include <QAction>
#include <QList>
#include <QObject>

#include "menu-plugin-iface.h"
#include "plugin-iface.h"

// 按需同步右键菜单插件：下载 / 释放空间 / 始终保留 / 取消始终保留。
// 动作与官方客户端对齐，实际执行交给 fnsync-ctl（控制 socket，不直接碰缓存）。
class FnSyncMenuPlugin : public QObject, public Peony::MenuPluginInterface
{
    Q_OBJECT
    Q_PLUGIN_METADATA(IID MenuPluginInterface_iid FILE "fnsync_menu.json")
    Q_INTERFACES(Peony::MenuPluginInterface)
public:
    explicit FnSyncMenuPlugin(QObject *parent = nullptr);

    PluginType pluginType() override;
    const QString name() override;
    const QString description() override;
    const QIcon icon() override;
    void setEnable(bool enable) override;
    bool isEnable() override;

    QString testPlugin() override;
    QList<QAction *> menuActions(Types types,
                                 const QString &uri,
                                 const QStringList &selectionUris) override;

private:
    bool m_enabled = true;
};

#endif // FNSYNC_MENU_PLUGIN_H
