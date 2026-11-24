"""fnsync_menu.menu_specs 的纯逻辑单元测试（无需 Nautilus）。"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.realpath(__file__)))
from fnsync_menu import menu_specs


def entry(path, state, pinned=False):
    return {"path": path, "state": state, "pinned": pinned}


def keys(entries):
    return [spec["key"] for spec in menu_specs(entries)]


def spec_of(entries, key):
    for spec in menu_specs(entries):
        if spec["key"] == key:
            return spec
    return None


class MenuSpecsTest(unittest.TestCase):
    def test_empty_selection(self):
        self.assertEqual(menu_specs([]), [])

    def test_outside_on_demand_mount(self):
        self.assertEqual(keys([entry("/tmp/a", None)]), [])

    def test_placeholder_file(self):
        specs = menu_specs([entry("/m/a.txt", "placeholder")])
        self.assertEqual([s["key"] for s in specs], ["download", "pin"])
        self.assertEqual(specs[0]["action"], "download")
        self.assertEqual(specs[0]["targets"], ["/m/a.txt"])

    def test_cached_unpinned_file(self):
        self.assertEqual(keys([entry("/m/a.txt", "cached")]), ["free", "pin"])

    def test_cached_pinned_file_has_no_free_or_pin(self):
        self.assertEqual(keys([entry("/m/a.txt", "cached", True)]), ["unpin"])

    def test_dirty_file_is_not_freeable(self):
        self.assertEqual(keys([entry("/m/a.txt", "dirty")]), ["pin"])

    def test_transferring_file_still_exposes_pin(self):
        for state in ("download", "upload", "queued", "releasing"):
            self.assertIn("pin", keys([entry("/m/a.txt", state)]), state)

    def test_error_does_not_hide_entries(self):
        # 失败只体现在弹窗；菜单按底层状态判断，入口必须保留。
        self.assertIn("download", keys([entry("/m/a.txt", "placeholder")]))

    def test_directory_exposes_bulk_items(self):
        self.assertEqual(
            keys([entry("/m/dir", "directory")]),
            ["download_dir", "free_dir", "pin_dir", "unpin_dir"],
        )

    def test_directory_targets_are_the_directory(self):
        for key in ("download_dir", "free_dir", "pin_dir", "unpin_dir"):
            spec = spec_of([entry("/m/dir", "directory")], key)
            self.assertEqual(spec["targets"], ["/m/dir"], key)

    def test_mixed_selection_keeps_file_and_directory_items(self):
        entries = [
            entry("/m/a.txt", "placeholder"),
            entry("/m/b.txt", "cached"),
            entry("/m/dir", "directory"),
        ]
        self.assertEqual(
            keys(entries),
            [
                "download", "download_dir",
                "free", "free_dir",
                "pin", "pin_dir",
                "unpin_dir",
            ],
        )

    def test_multiple_files_share_one_item(self):
        entries = [
            entry("/m/a.txt", "placeholder"),
            entry("/m/b.txt", "placeholder"),
        ]
        spec = spec_of(entries, "download")
        self.assertEqual(spec["targets"], ["/m/a.txt", "/m/b.txt"])

    def test_duplicate_paths_are_deduplicated(self):
        entries = [
            entry("/m/a.txt", "placeholder"),
            entry("/m/a.txt", "placeholder"),
        ]
        spec = spec_of(entries, "download")
        self.assertEqual(spec["targets"], ["/m/a.txt"])

    def test_pinned_and_unpinned_files_split_targets(self):
        entries = [
            entry("/m/a.txt", "cached", True),
            entry("/m/b.txt", "cached", False),
        ]
        self.assertEqual(spec_of(entries, "pin")["targets"], ["/m/b.txt"])
        self.assertEqual(spec_of(entries, "unpin")["targets"], ["/m/a.txt"])


if __name__ == "__main__":
    unittest.main()
