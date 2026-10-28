# /// script
# dependencies = ["argon2-cffi==25.1.0"]
# ///
"""Generate a Rauthy bootstrap password hash without echoing the password."""
from getpass import getpass
from argon2 import PasswordHasher
password = getpass("Login password: ")
if len(password) < 16:
    raise SystemExit("Use at least 16 characters.")
print(PasswordHasher(time_cost=3, memory_cost=32768, parallelism=2).hash(password))
