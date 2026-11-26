#ifndef FNSYNC_EMBLEM_PLUGIN_H
#define FNSYNC_EMBLEM_PLUGIN_H

#include <QObject>

#include "emblem-plugin-iface.h"
#include "plugin-iface.h"

// Peony 的角标插件入口：PluginManager 加载后调用 create() 取得
// EmblemProvider 并注册到 EmblemProviderManager。
class FnSyncEmblemPlugin : public QObject, public Peony::EmblemPluginInterface
{
    Q_OBJECT
    Q_PLUGIN_METADATA(IID EmblemPluginInterface_iid FILE "fnsync_emblem.json")
    Q_INTERFACES(Peony::EmblemPluginInterface)
public:
    explicit FnSyncEmblemPlugin(QObject *parent = nullptr);

    PluginType pluginType() override;
    const QString name() override;
    const QString description() override;
    const QIcon icon() override;
    void setEnable(bool enable) override;
    bool isEnable() override;

    Peony::EmblemProvider *create() override;

private:
    bool m_enabled = true;
    Peony::EmblemProvider *m_provider = nullptr;
};

#endif // FNSYNC_EMBLEM_PLUGIN_H
