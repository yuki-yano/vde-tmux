#!/usr/bin/python3

import sys
import time


delay = float(sys.argv[1]) if len(sys.argv) > 1 else 600.0
time.sleep(delay)
