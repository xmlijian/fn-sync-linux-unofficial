// 飞牛同步 UOS/DDE 文件管理器扩展：
//   - 右键菜单：下载到本地 / 释放空间 / 始终保留 / 取消保留（文件与文件夹）
//   - 状态角标：灰云=未下载、绿云=已下载、蓝=下载中、上箭头=上传中、橙=已修改
//
// dfm-extension 是纯 C++ 接口（无 Qt），插件以共享库形式被
// dde-file-manager 从 /usr/lib/<multiarch>/dde-file-manager/plugins/extensions/
// 扫描加载，导出 dfm_extension_* 四个 C 符号。

#include "fnsync_dde_status.h"

#include <dfm-extension/dfm-extension.h>
// dfm-extension.h 只引入插件基类，DFMExtMenu/DFMExtAction 等需显式包含。
#include <dfm-extension/emblemicon/dfmextemblem.h>
#include <dfm-extension/emblemicon/dfmextemblemiconlayout.h>
#include <dfm-extension/menu/dfmextaction.h>
#include <dfm-extension/menu/dfmextmenu.h>
#include <dfm-extension/menu/dfmextmenuproxy.h>

#include <fcntl.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

#include <algorithm>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <map>
#include <string>
#include <vector>

namespace {

const char kCtl[] = "/usr/bin/fnsync-ctl";
const char kActionKey[] = "fnsync-action";
const char kTargetsKey[] = "fnsync-targets";

struct MenuSpec
{
    std::string label;
    std::string action;
    std::vector<std::string> targets;
    std::string icon;
};

int hexValue(char c)
{
    if (c >= '0' && c <= '9')
        return c - '0';
    if (c >= 'a' && c <= 'f')
        return c - 'a' + 10;
    if (c >= 'A' && c <= 'F')
        return c - 'A' + 10;
    return -1;
}

// DDE 传入的路径可能是 URL（file:///home/...%20...），统一转成本地绝对路径。
std::string toLocalPath(const std::string &path)
{
    std::string result = path;
    if (result.compare(0, 7, "file://") == 0)
        result = result.substr(7);

    std::string decoded;
    decoded.reserve(result.size());
    for (size_t i = 0; i < result.size(); ++i) {
        if (result[i] == '%' && i + 2 < result.size()) {
            const int hi = hexValue(result[i + 1]);
            const int lo = hexValue(result[i + 2]);
            if (hi >= 0 && lo >= 0) {
                decoded.push_back(char(hi * 16 + lo));
                i += 2;
                continue;
            }
        }
        decoded.push_back(result[i]);
    }
    return decoded;
}

bool isDirectory(const std::string &path)
{
    struct stat st;
    return ::stat(path.c_str(), &st) == 0 && S_ISDIR(st.st_mode);
}

std::string join(const std::vector<std::string> &items, char separator)
{
    std::string result;
    for (size_t i = 0; i < items.size(); ++i) {
        if (i)
            result.push_back(separator);
        result += items[i];
    }
    return result;
}

std::vector<std::string> split(const std::string &text, char separator)
{
    std::vector<std::string> parts;
    size_t start = 0;
    while (start <= text.size()) {
        const size_t end = text.find(separator, start);
        if (end == std::string::npos) {
            parts.push_back(text.substr(start));
            break;
        }
        parts.push_back(text.substr(start, end - start));
        start = end + 1;
    }
    return parts;
}

// 运行 fnsync-ctl（与 Peony/Nautilus 扩展一致，GUI 提示由 ctl 自己决定）。
void runCtl(const std::string &action, const std::vector<std::string> &targets)
{
    if (action.empty())
        return;
    fnsyncDebug("trigger " + action + " " + join(targets, ' '));

    pid_t pid = ::fork();
    if (pid < 0)
        return;
    if (pid == 0) {
        // 子进程：脱离文件管理器，避免僵尸进程。
        ::setsid();
        ::setenv("FNSYNC_GUI", "1", 1);
        pid_t grandchild = ::fork();
        if (grandchild < 0)
            ::_exit(1);
        if (grandchild > 0)
            ::_exit(0);

        std::vector<char *> argv;
        argv.push_back(const_cast<char *>(kCtl));
        argv.push_back(const_cast<char *>(action.c_str()));
        for (const std::string &target : targets)
            argv.push_back(const_cast<char *>(target.c_str()));
        argv.push_back(nullptr);
        ::execv(kCtl, argv.data());
        ::_exit(1);
    }
    int status = 0;
    ::waitpid(pid, &status, 0);
}

const char *iconForState(const std::string &state)
{
    if (state == "placeholder")
        return "emblem-fnsync-online";
    if (state == "cached")
        return "emblem-fnsync-local";
    if (state == "download" || state == "queued")
        return "emblem-fnsync-downloading";
    if (state == "upload")
        return "emblem-fnsync-uploading";
    if (state == "dirty" || state == "modified")
        return "emblem-fnsync-modified";
    if (state == "releasing")
        return "emblem-fnsync-online";
    return "";
}

// 与 scripts/desktop/fnsync_menu.py 的 menu_specs() 保持一致。
std::vector<MenuSpec> menuSpecs(const std::vector<FnSyncState> &states,
                                const std::vector<std::string> &paths)
{
    std::vector<std::string> dirs;
    std::vector<size_t> files;
    for (size_t i = 0; i < paths.size(); ++i) {
        if (states[i].state == "directory")
            dirs.push_back(paths[i]);
        else
            files.push_back(i);
    }

    std::vector<MenuSpec> specs;
    std::vector<std::string> download;
    for (size_t index : files) {
        if (states[index].state == "placeholder")
            download.push_back(paths[index]);
    }
    if (!download.empty())
        specs.push_back({std::string("下载到本地"), "download", download,
                         "emblem-fnsync-downloading"});
    if (!dirs.empty())
        specs.push_back({std::string("下载此文件夹"), "download", dirs,
                         "emblem-fnsync-downloading"});

    std::vector<std::string> free;
    for (size_t index : files) {
        if (states[index].state == "cached" && !states[index].pinned)
            free.push_back(paths[index]);
    }
    if (!free.empty())
        specs.push_back({std::string("释放本地副本"), "free", free,
                         "emblem-fnsync-online"});
    if (!dirs.empty())
        specs.push_back({std::string("释放此文件夹"), "free", dirs,
                         "emblem-fnsync-online"});

    std::vector<std::string> pin;
    for (size_t index : files) {
        if (!states[index].pinned)
            pin.push_back(paths[index]);
    }
    if (!pin.empty())
        specs.push_back({std::string("始终保留"), "pin", pin,
                         "emblem-fnsync-local"});
    if (!dirs.empty())
        specs.push_back({std::string("保留此文件夹"), "pin", dirs,
                         "emblem-fnsync-local"});

    std::vector<std::string> unpin;
    for (size_t index : files) {
        if (states[index].pinned)
            unpin.push_back(paths[index]);
    }
    if (!unpin.empty())
        specs.push_back({std::string("取消始终保留"), "unpin", unpin,
                         "emblem-fnsync-local"});
    if (!dirs.empty())
        specs.push_back({std::string("取消文件夹保留"), "unpin", dirs,
                         "emblem-fnsync-local"});

    return specs;
}

// 状态变化后让文件管理器重新查询角标：保持时间戳不变的 utimensat
// 只改变 ctime，产生一次 IN_ATTRIB（DDE 的文件视图据此刷新图标）。
void touchAttributes(const std::string &path)
{
    struct stat st;
    if (::stat(path.c_str(), &st) != 0)
        return;
    struct timespec times[2];
    times[0] = st.st_atim;
    times[1] = st.st_mtim;
    ::utimensat(AT_FDCWD, path.c_str(), times, 0);
}

class FnSyncMenuPlugin : public DFMEXT::DFMExtMenuPlugin
{
public:
    FnSyncMenuPlugin()
    {
        // dfm-extension 的基类方法不是 virtual（库里只有一个实现），
        // 官方用法是 register* 注册回调。
        registerInitialize([this](DFMEXT::DFMExtMenuProxy *proxy) {
            m_proxy = proxy;
            fnsyncDebug("menu initialize");
        });
        registerBuildNormalMenu([this](DFMEXT::DFMExtMenu *main,
                                       const std::string &currentPath,
                                       const std::string &focusPath,
                                       const std::list<std::string> &pathList,
                                       bool onDesktop) {
            (void)currentPath;
            (void)focusPath;
            (void)onDesktop;
            return buildMenu(main, pathList);
        });
    }

private:
    bool buildMenu(DFMEXT::DFMExtMenu *main, const std::list<std::string> &pathList)
    {
        if (!main || !m_proxy)
            return false;
        if (pathList.empty())
            return false;

        std::vector<std::string> paths;
        for (const std::string &item : pathList) {
            const std::string path = toLocalPath(item);
            if (path.empty())
                continue;
            if (std::find(paths.begin(), paths.end(), path) == paths.end())
                paths.push_back(path);
        }
        if (paths.empty())
            return false;
        fnsyncDebug("menu build paths=" + std::to_string(paths.size()));

        FnSyncStatusClient &client = FnSyncStatusClient::instance();
        client.refreshNow();

        std::vector<std::string> filtered;
        std::vector<FnSyncState> states;
        for (const std::string &path : paths) {
            FnSyncState state;
            if (!client.lookup(path, &state)) {
                // 刚建好的子目录可能还不在快照里；只要在挂载点内就按目录处理。
                if (!client.contains(path) || !isDirectory(path))
                    continue;
                state.state = "directory";
            }
            filtered.push_back(path);
            states.push_back(state);
        }
        if (states.empty())
            return false;

        const std::vector<MenuSpec> specs = menuSpecs(states, filtered);
        if (specs.empty())
            return false;

        // DDE 的右键菜单宽度有限，条目带“飞牛同步：”前缀会被中间省略。
        // 改成“飞牛同步”子菜单，条目本身保持短文案。
        DFMEXT::DFMExtMenu *submenu = m_proxy->createMenu();
        DFMEXT::DFMExtAction *entry = m_proxy->createAction();
        if (!submenu || !entry)
            return false;
        entry->setText("飞牛同步");
        entry->setIcon("emblem-fnsync-local");

        for (const MenuSpec &spec : specs) {
            DFMEXT::DFMExtAction *action = m_proxy->createAction();
            if (!action)
                continue;
            action->setText(spec.label);
            if (!spec.icon.empty())
                action->setIcon(spec.icon);
            action->setProperty(kActionKey, spec.action);
            action->setProperty(kTargetsKey, join(spec.targets, '\n'));
            action->registerTriggered([](DFMEXT::DFMExtAction *self, bool checked) {
                (void)checked;
                if (!self)
                    return;
                runCtl(self->property(kActionKey), split(self->property(kTargetsKey), '\n'));
            });
            submenu->addAction(action);
        }
        entry->setMenu(submenu);
        main->addAction(entry);
        return true;
    }

private:
    DFMEXT::DFMExtMenuProxy *m_proxy = nullptr;
};

class FnSyncEmblemPlugin : public DFMEXT::DFMExtEmblemIconPlugin
{
public:
    FnSyncEmblemPlugin()
    {
        registerLocationEmblemIcons([](const std::string &filePath, int systemIconCount) {
            (void)systemIconCount;
            DFMEXT::DFMExtEmblem emblem;
            FnSyncState state;
            const std::string path = toLocalPath(filePath);
            if (!FnSyncStatusClient::instance().lookup(path, &state) || state.state == "directory")
                return emblem;

            const char *icon = iconForState(state.state);
            if (icon && *icon) {
                std::vector<DFMEXT::DFMExtEmblemIconLayout> layouts;
                layouts.emplace_back(DFMEXT::DFMExtEmblemIconLayout::LocationType::BottomRight, icon);
                emblem.setEmblem(layouts);
                fnsyncDebug(std::string("emblem ") + path + " -> " + icon + " (" + state.state + ")");
            }
            return emblem;
        });
    }
};

FnSyncMenuPlugin g_menuPlugin;
FnSyncEmblemPlugin g_emblemPlugin;

bool g_initialized = false;

void onStatesChanged(const std::vector<std::string> &changed)
{
    for (const std::string &path : changed)
        touchAttributes(path);
}

} // namespace

// dfm-extension 加载器按这四个（拼写就是这么写，来自官方头文件）C 符号取插件。
namespace DFMEXT {

extern "C" void dfm_extension_initiliaze()
{
    if (g_initialized)
        return;
    g_initialized = true;
    FnSyncStatusClient::instance().setStatesChanged(onStatesChanged);
    FnSyncStatusClient::instance().startPolling();
    fnsyncDebug("extension initialize");
}

extern "C" void dfm_extension_shutdown()
{
    fnsyncDebug("extension shutdown");
}

extern "C" DFMExtMenuPlugin *dfm_extension_menu()
{
    return &g_menuPlugin;
}

extern "C" DFMExtEmblemIconPlugin *dfm_extension_emblem()
{
    return &g_emblemPlugin;
}

extern "C" DFMExtWindowPlugin *dfm_extension_window()
{
    return nullptr;
}

} // namespace DFMEXT
