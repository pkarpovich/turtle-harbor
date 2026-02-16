import sys
import time
import requests

def main():
    print(f"Python {sys.version}")
    print(f"requests {requests.__version__}")
    print(f"Virtual env: {sys.prefix}")
    print()

    while True:
        try:
            resp = requests.get("https://httpbin.org/get", timeout=10)
            print(f"[{time.strftime('%H:%M:%S')}] GET httpbin.org -> {resp.status_code}")
        except requests.RequestException as e:
            print(f"[{time.strftime('%H:%M:%S')}] Error: {e}")
        time.sleep(30)

if __name__ == "__main__":
    main()
