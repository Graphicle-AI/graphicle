import os
import unittest
from pathlib import Path
from unittest.mock import patch

from graphicle_sdk.client import default_data_dir


class DefaultDataDirTests(unittest.TestCase):
    def test_windows_uses_local_app_data(self):
        local_app_data = Path.cwd() / "profile" / "AppData" / "Local"
        with patch("graphicle_sdk.client.sys.platform", "win32"), patch.dict(
            os.environ,
            {"GRAPHICLE_DATA_DIR": "", "LOCALAPPDATA": str(local_app_data)},
            clear=True,
        ):
            self.assertEqual(default_data_dir(), local_app_data / "Graphicle")

    def test_windows_falls_back_to_local_app_data_under_home(self):
        home = Path.cwd() / "profile"
        with patch("graphicle_sdk.client.sys.platform", "win32"), patch.object(
            Path, "home", return_value=home
        ), patch.dict(
            os.environ,
            {"GRAPHICLE_DATA_DIR": "", "LOCALAPPDATA": "relative"},
            clear=True,
        ):
            self.assertEqual(default_data_dir(), home / "AppData" / "Local" / "Graphicle")


if __name__ == "__main__":
    unittest.main()
