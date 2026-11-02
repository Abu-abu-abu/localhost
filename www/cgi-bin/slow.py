import time

# Проверка таймаута: сервер должен убить скрипт и вернуть 504.
time.sleep(30)
print("Content-Type: text/plain")
print()
print("слишком поздно")
