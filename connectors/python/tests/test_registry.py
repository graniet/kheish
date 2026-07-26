import sys
import unittest
from pathlib import Path


CONNECTOR_ROOT = Path(__file__).resolve().parents[1]
if str(CONNECTOR_ROOT) not in sys.path:
    sys.path.insert(0, str(CONNECTOR_ROOT))

import platforms  # noqa: E402
import registry  # noqa: E402


class ConnectorRegistryTests(unittest.TestCase):
    def test_registry_exposes_every_bundled_connector(self) -> None:
        expected = {
            "discord",
            "email",
            "matrix",
            "signal",
            "sms",
            "webhook",
            "whatsapp",
        }

        self.assertEqual(set(registry.CONNECTOR_FACTORIES), expected)
        for name, factory in registry.CONNECTOR_FACTORIES.items():
            self.assertEqual(factory.platform, name)
            self.assertTrue(factory.__module__.startswith("adapters."))

    def test_legacy_platforms_module_reexports_the_registry_and_classes(self) -> None:
        self.assertIs(platforms.CONNECTOR_FACTORIES, registry.CONNECTOR_FACTORIES)
        for factory in registry.CONNECTOR_FACTORIES.values():
            self.assertIs(getattr(platforms, factory.__name__), factory)


if __name__ == "__main__":
    unittest.main()
