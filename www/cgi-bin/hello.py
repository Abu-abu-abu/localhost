import os
import sys

# Тело запроса приходит в stdin до EOF.
body = sys.stdin.read()

print("Content-Type: text/plain; charset=utf-8")
print()
print("Hello from CGI")
for key in (
    "REQUEST_METHOD",
    "QUERY_STRING",
    "PATH_INFO",
    "SCRIPT_NAME",
    "CONTENT_LENGTH",
    "CONTENT_TYPE",
    "SERVER_NAME",
    "SERVER_PORT",
    "HTTP_COOKIE",
):
    print(key + "=" + os.environ.get(key, ""))
print("cwd=" + os.getcwd())
print("argv=" + repr(sys.argv))
print("body=" + body)
