#
# Fishball P25 - Build configurations
#
# SPDX-License-Identifier: MIT
#

from .config import P25Config


def default():
    """Default P25 configuration for Fishball Z7020"""
    return P25Config()
