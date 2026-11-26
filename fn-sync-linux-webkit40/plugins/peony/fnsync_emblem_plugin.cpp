#include "fnsync_emblem_plugin.h"

#include "fnsync_emblem_provider.h"

FnSyncEmblemPlugin::FnSyncEmblemPlugin(QObject *parent)
    : QObject(parent)
{
}

Peony::PluginInterface::PluginType FnSyncEmblemPlugin::pluginType()
{
    return PluginType::EmblemPlugin;
}

const QString FnSyncEmblemPlugin::name()
{
    return QStringLiteral("飞牛同步");
}

const QString FnSyncEmblemPlugin::description()
{
    return QStringLiteral("显示按需同步文件的下载/保留状态");
}

const QIcon FnSyncEmblemPlugin::icon()
{
    return QIcon::fromTheme(QStringLiteral("emblem-fnsync-local"));
}

void FnSyncEmblemPlugin::setEnable(bool enable)
{
    m_enabled = enable;
}

bool FnSyncEmblemPlugin::isEnable()
{
    return m_enabled;
}

Peony::EmblemProvider *FnSyncEmblemPlugin::create()
{
    if (!m_provider)
        m_provider = new FnSyncEmblemProvider(this);
    return m_provider;
}
