#include "fnsync_menu_plugin.h"

#include <QFileInfo>
#include <QProcess>
#include <QSet>
#include <QUrl>

#include "fnsync_status_client.h"

namespace {

const QString kPrefix = QStringLiteral("飞牛同步：");
const QString kCtl = QStringLiteral("/usr/bin/fnsync-ctl");

struct MenuSpec
{
    QString label;
    QString action;
    QStringList targets;
    QString iconName;
};

QString localPathOf(const QString &uri)
{
    if (uri.startsWith(QLatin1String("file://")))
        return QUrl(uri).toLocalFile();
    return uri;
}

void runCtl(const QString &action, const QStringList &targets)
{
    // 与 Nautilus 扩展一致：GUI 入口由 ctl 决定是否弹进度/错误提示。
    qputenv("FNSYNC_GUI", "1");
    QProcess::startDetached(kCtl, QStringList(action) + targets);
}

// 与 scripts/desktop/fnsync_menu.py 的 menu_specs() 保持一致。
QList<MenuSpec> menuSpecs(const QList<FnSyncState> &states, const QStringList &paths)
{
    QStringList dirs;
    QList<int> files;
    for (int i = 0; i < paths.size(); ++i) {
        if (states.at(i).state == QLatin1String("directory"))
            dirs.append(paths.at(i));
        else
            files.append(i);
    }

    QList<MenuSpec> specs;

    QStringList download;
    for (int index : files) {
        if (states.at(index).state == QLatin1String("placeholder"))
            download.append(paths.at(index));
    }
    if (!download.isEmpty())
        specs.append({kPrefix + QStringLiteral("下载到本地"),
                      QStringLiteral("download"), download,
                      QStringLiteral("emblem-fnsync-downloading")});
    if (!dirs.isEmpty())
        specs.append({kPrefix + QStringLiteral("下载此文件夹"),
                      QStringLiteral("download"), dirs,
                      QStringLiteral("emblem-fnsync-downloading")});

    QStringList free;
    for (int index : files) {
        if (states.at(index).state == QLatin1String("cached") && !states.at(index).pinned)
            free.append(paths.at(index));
    }
    if (!free.isEmpty())
        specs.append({kPrefix + QStringLiteral("释放空间（只删本地副本）"),
                      QStringLiteral("free"), free,
                      QStringLiteral("emblem-fnsync-online")});
    if (!dirs.isEmpty())
        specs.append({kPrefix + QStringLiteral("释放此文件夹（只删本地副本）"),
                      QStringLiteral("free"), dirs,
                      QStringLiteral("emblem-fnsync-online")});

    QStringList pin;
    for (int index : files) {
        if (!states.at(index).pinned)
            pin.append(paths.at(index));
    }
    if (!pin.isEmpty())
        specs.append({kPrefix + QStringLiteral("始终保留在此电脑"),
                      QStringLiteral("pin"), pin,
                      QStringLiteral("emblem-fnsync-local")});
    if (!dirs.isEmpty())
        specs.append({kPrefix + QStringLiteral("保留并下载此文件夹（当前文件）"),
                      QStringLiteral("pin"), dirs,
                      QStringLiteral("emblem-fnsync-local")});

    QStringList unpin;
    for (int index : files) {
        if (states.at(index).pinned)
            unpin.append(paths.at(index));
    }
    if (!unpin.isEmpty())
        specs.append({kPrefix + QStringLiteral("取消始终保留"),
                      QStringLiteral("unpin"), unpin,
                      QStringLiteral("emblem-fnsync-local")});
    if (!dirs.isEmpty())
        specs.append({kPrefix + QStringLiteral("取消文件夹内所有保留"),
                      QStringLiteral("unpin"), dirs,
                      QStringLiteral("emblem-fnsync-local")});

    return specs;
}

} // namespace

FnSyncMenuPlugin::FnSyncMenuPlugin(QObject *parent)
    : QObject(parent)
{
    fnsyncDebug(QStringLiteral("menu plugin constructed"));
}

Peony::PluginInterface::PluginType FnSyncMenuPlugin::pluginType()
{
    return PluginType::MenuPlugin;
}

const QString FnSyncMenuPlugin::name()
{
    return QStringLiteral("飞牛同步");
}

const QString FnSyncMenuPlugin::description()
{
    return QStringLiteral("按需同步文件的下载/释放/保留操作");
}

const QIcon FnSyncMenuPlugin::icon()
{
    return QIcon::fromTheme(QStringLiteral("emblem-fnsync-local"));
}

void FnSyncMenuPlugin::setEnable(bool enable)
{
    m_enabled = enable;
}

bool FnSyncMenuPlugin::isEnable()
{
    return m_enabled;
}

QString FnSyncMenuPlugin::testPlugin()
{
    return QStringLiteral("fnsync-menu");
}

QList<QAction *> FnSyncMenuPlugin::menuActions(Types types,
                                               const QString &uri,
                                               const QStringList &selectionUris)
{
    QList<QAction *> actions;
    if (!m_enabled)
        return actions;
    fnsyncDebug(QStringLiteral("menuActions types=%1 uri=%2 selection=%3").arg(int(types)).arg(uri).arg(selectionUris.size()));
    if (!types.testFlag(Peony::MenuPluginInterface::DirectoryView))
        return actions;

    QStringList raw = selectionUris;
    if (raw.isEmpty())
        return actions; // 空白区域不展示飞牛同步菜单

    QStringList paths;
    QSet<QString> seen;
    for (const QString &item : raw) {
        const QString path = localPathOf(item);
        if (path.isEmpty() || seen.contains(path))
            continue;
        seen.insert(path);
        paths.append(path);
    }
    if (paths.isEmpty())
        return actions;

    // 右键时刻同步刷新一次状态，避免菜单基于 1.5s 前的旧快照。
    FnSyncStatusClient *client = FnSyncStatusClient::instance();
    client->refreshNow();

    QList<int> indices;
    QList<FnSyncState> states;
    for (int i = 0; i < paths.size(); ++i) {
        FnSyncState state;
        if (!client->lookup(paths.at(i), &state)) {
            // 刚建好的子目录可能还没出现在状态快照里；只要在挂载点内，
            // 按目录处理，让文件夹级动作可用。
            if (!client->contains(paths.at(i)) || !QFileInfo(paths.at(i)).isDir())
                continue;
            state = FnSyncState{QStringLiteral("directory"), false};
        }
        indices.append(i);
        states.append(state);
    }
    if (states.isEmpty())
        return actions;

    QStringList filtered;
    for (int index : indices)
        filtered.append(paths.at(index));
    fnsyncDebug(QStringLiteral("menu states=%1/%2 first=%3 state=%4")
                .arg(states.size()).arg(paths.size()).arg(paths.value(0)).arg(states.value(0).state));

    for (const MenuSpec &spec : menuSpecs(states, filtered)) {
        QAction *action = new QAction(QIcon::fromTheme(spec.iconName), spec.label, nullptr);
        const QString verb = spec.action;
        const QStringList targetsCopy = spec.targets;
        // 以 action 自身作为 context：Qt5 不允许 receiver 为 nullptr 的四参数 connect。
        QObject::connect(action, &QAction::triggered, action,
                         [verb, targetsCopy]() {
                             fnsyncDebug(QStringLiteral("trigger %1 %2").arg(verb, targetsCopy.join(' ')));
                             runCtl(verb, targetsCopy);
                         });
        actions.append(action);
    }
    return actions;
}
