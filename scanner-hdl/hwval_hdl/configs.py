#
# Fishball hardware validation (hwval) - Build configurations
#
# Mirrors radio_core/configs.py: each configuration is a function that
# returns an HwvalConfig. The ``configs`` dict maps names to them.
#
# SPDX-License-Identifier: MIT
#

from .config import HwvalConfig, default, configs  # noqa: F401
