"""mitmproxy addon that writes structured JSON request logs.

Usage: mitmdump -s bench/mitmproxy-addon.py --set logfile=proxy.jsonl
"""

import json
import time

from mitmproxy import ctx


class RequestLogger:
    def __init__(self):
        self.logfile = None

    def load(self, loader):
        loader.add_option(
            name="logfile",
            typespec=str,
            default="proxy.jsonl",
            help="Path to write JSON request log",
        )

    def configure(self, updated):
        if "logfile" in updated:
            if self.logfile:
                self.logfile.close()
            self.logfile = open(ctx.options.logfile, "a")

    def response(self, flow):
        if not self.logfile:
            return

        entry = {
            "timestamp": time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime()),
            "method": flow.request.method,
            "host": flow.request.host,
            "url": flow.request.path,
            "request_bytes": len(flow.request.raw_content) if flow.request.raw_content else 0,
            "response_bytes": len(flow.response.raw_content) if flow.response.raw_content else 0,
            "status": flow.response.status_code,
            "duration_ms": int((flow.response.timestamp_end - flow.request.timestamp_start) * 1000),
        }

        self.logfile.write(json.dumps(entry) + "\n")
        self.logfile.flush()

    def done(self):
        if self.logfile:
            self.logfile.close()


addons = [RequestLogger()]
