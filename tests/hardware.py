import importlib.util
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "hardware", Path(__file__).resolve().parents[1] / "deploy/hardware.py"
)
hardware = importlib.util.module_from_spec(spec)
spec.loader.exec_module(hardware)


class HardwareTests(unittest.TestCase):
    def test_nvme_and_ata(self):
        result = hardware.normalize(
            {
                "temperature": {"current": 0},
                "smart_status": {"passed": False},
                "nvme_smart_health_information_log": {"percentage_used": 105},
                "ata_smart_attributes": {"table": [{"id": 197, "raw": {"value": 3}}]},
            }
        )
        self.assertEqual(result["temperature"], 0)
        self.assertFalse(result["passed"])
        self.assertEqual(result["percentage_used"], 105)
        self.assertEqual(result["pending"], 3)
        self.assertIsNone(result["reallocated"])

    def test_exit_mask_and_failures(self):
        with patch.object(hardware.subprocess, "run") as run:
            run.return_value = subprocess.CompletedProcess(
                [], 8, b'{"smartctl":{"exit_status":8}}'
            )
            result = hardware.read_disk("sda")
            self.assertFalse(result["passed"])
            self.assertNotIn("error", result)
            run.return_value = subprocess.CompletedProcess([], 2, b"{}")
            self.assertIn("error", hardware.read_disk("sda"))
            run.side_effect = subprocess.TimeoutExpired("smartctl", 10)
            self.assertIn("超时", hardware.read_disk("sda")["error"])
            run.side_effect = FileNotFoundError()
            self.assertIn("未安装", hardware.read_disk("sda")["error"])

    def test_partition_and_stacked_device(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            disk = root / "sda"
            (disk / "device").mkdir(parents=True)
            partition = disk / "sda1"
            partition.mkdir()
            (partition / "partition").touch()
            slaves = root / "dm-0/slaves"
            slaves.mkdir(parents=True)
            (slaves / "sda1").symlink_to(partition)
            self.assertEqual(hardware.physical(root / "dm-0"), ["sda"])
            self.assertEqual(hardware.physical(partition), ["sda"])
            self.assertEqual(hardware.physical(root / "virtual"), [])


if __name__ == "__main__":
    unittest.main()
