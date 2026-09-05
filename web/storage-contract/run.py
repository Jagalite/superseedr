#!/usr/bin/env python3
"""Serve and run the production OPFS contract in headless Chrome; exit nonzero on failure."""
import argparse
import functools
import http.server
import json
import os
import pathlib
import queue
import shutil
import subprocess
import tempfile
import threading

parser=argparse.ArgumentParser()
parser.add_argument('--browser', default=os.environ.get('SUPERSEEDR_BROWSER_BIN') or shutil.which('chromium') or shutil.which('google-chrome') or '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome')
args=parser.parse_args()
results=queue.Queue()
class Handler(http.server.SimpleHTTPRequestHandler):
    def log_message(self,*args): pass
    def do_POST(self):
        if self.path != '/result': self.send_error(404); return
        length=int(self.headers.get('Content-Length','0'))
        if length>1024*1024: self.send_error(413);return
        results.put(json.loads(self.rfile.read(length)))
        self.send_response(200);self.end_headers()
root=pathlib.Path(__file__).resolve().parent
with http.server.ThreadingHTTPServer(('127.0.0.1',0),functools.partial(Handler,directory=str(root))) as server, tempfile.TemporaryDirectory(prefix='superseedr-opfs-browser-') as profile:
    threading.Thread(target=server.serve_forever,daemon=True).start()
    with tempfile.TemporaryFile() as log:
        browser=subprocess.Popen([args.browser,'--headless=new','--disable-gpu','--no-first-run','--no-default-browser-check',f'--user-data-dir={profile}',f'http://127.0.0.1:{server.server_port}/index.html'],stdout=log,stderr=log)
        try:
            result=results.get(timeout=180)
            print(json.dumps(result,indent=2))
            if not result.get('ok'): raise SystemExit(1)
        except queue.Empty:
            log.seek(0);print(log.read().decode(errors='replace')[-8000:]);raise SystemExit('Browser contract timed out')
        finally:
            browser.terminate()
            try:browser.wait(timeout=10)
            except subprocess.TimeoutExpired:browser.kill();browser.wait()
            server.shutdown()
