"""Export Rauthy discovery with its supported public PKCE method included."""
import argparse
import json
from pathlib import Path
from urllib.request import urlopen
parser=argparse.ArgumentParser()
parser.add_argument('url',help='Rauthy HTTPS OpenID discovery URL')
parser.add_argument('output',type=Path)
args=parser.parse_args()
if not args.url.startswith('https://'):parser.error('HTTPS is required')
with urlopen(args.url,timeout=20) as response:
    data=json.loads(response.read(262145))
methods=data.setdefault('token_endpoint_auth_methods_supported',[])
if 'none' not in methods:methods.insert(0,'none')
args.output.write_text(json.dumps(data),encoding='utf-8')
