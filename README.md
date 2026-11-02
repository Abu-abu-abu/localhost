# LocalHost

Учебный HTTP/1.1 сервер на Rust. Сервер работает в одном процессе и одном потоке, использует `epoll` на Linux и `WSAPoll` на Windows, обслуживает несколько адресов/портов, а клиентские сокеты открывает в неблокирующем режиме.

## Запуск

На Windows:

```powershell
cargo run -- --config localhost.conf
```

На Linux:

```bash
cargo run -- --config localhost.linux.conf
```

После запуска откройте `http://127.0.0.1:8080/`. Завершение — `Ctrl+C`.

## Формат конфигурации

Каждая директива занимает одну строку. Значения не заключаются в кавычки; пробелы в путях пока не поддерживаются. Пути считаются относительно файла конфигурации.

```text
server address=127.0.0.1 port=8080 name=localhost root=www max_body=1048576 index=index.html listing=false
route / methods=GET,POST,DELETE root=www
route /cgi-bin methods=GET,POST root=www/cgi-bin
route /uploads methods=GET,POST,DELETE root=www/uploads upload=true listing=true
route /old redirect=/ status=301
error 404=errors/404.html
error 504=errors/504.html
cgi .py=py
```

Повторные блоки `server` с одним `address` и `port`, но разными именами, делят один слушающий сокет. Первый блок будет вариантом по умолчанию, если заголовок `Host` не совпал. `route` и `error` относятся к последнему объявленному серверу. Для загрузки multipart используйте поле формы с `name="file"`; обычный POST сохраняется под именем из URL. Загружать разрешено только на маршруты с `upload=true`.

`localhost.conf` предназначен для Windows и использует Python Launcher `py`. Для Linux используйте готовый `localhost.linux.conf`, где CGI запускается через `python3`. Скрипты для проверки находятся в `www/cgi-bin/`.

## Запуск на Windows

```powershell
cargo test --offline
cargo run --offline -- --config localhost.conf
```

Оставьте сервер в первом окне терминала. Откройте второе окно для команд проверки ниже.

## Запуск на Linux (университетский компьютер)

```bash
python3 --version
cargo test
cargo run -- --config localhost.linux.conf
```

При первой сборке Cargo может скачать зависимость `libc`. После запуска оставьте сервер в этом окне и выполняйте команды проверки во втором терминале.

## Что реализовано

- HTTP/1.1 запросы с `Content-Length` и `Transfer-Encoding: chunked`, GET/POST/DELETE, keep-alive до 100 запросов на соединение, проверка размера тела и ограничение времени ожидания.
- Статические файлы, index-файл, необязательный просмотр каталогов, маршруты с методами, перенаправления, загрузка multipart и обычного тела, DELETE.
- Коды ошибок 400, 403, 404, 405, 413, 500 и 504, настраиваемые страницы ошибок, cookie с короткой серверной сессией.
- Несколько virtual host на одном адресе и несколько слушающих адресов/портов.
- CGI для настроенного расширения: отдельный процесс с переменными CGI, телом через stdin, ограничением времени и ответом 504 при тайм-ауте.
- Модульные тесты для разбора HTTP, chunked body, конфигурации, выбора маршрута и кодов ответа.

## Известные ограничения

CGI-вывод и обычные статические файлы читаются из временных файлов/диска синхронно; сеть остаётся неблокирующей. Keep-alive поддерживается, но TLS, WebSocket и исчерпывающая проверка всех вариантов HTTP/1.1 не реализованы. Конфигурация использует собственный упрощённый формат, не синтаксис NGINX. Полноценные стресс-тесты, Linux-проверка epoll, сравнение с NGINX и проверка утечек памяти ещё не проведены.

## Проверка во втором терминале

Стартовая страница должна вернуть `200 OK`:

```bash
curl -i http://127.0.0.1:8080/
```

Проверьте повторное использование соединения. В выводе ищите `Re-using existing connection`:

```bash
curl --http1.1 -v http://127.0.0.1:8080/ http://127.0.0.1:8080/index.html
```

Проверьте CGI. `hello.py` должен вернуть `200 OK` и показать параметры запроса:

```bash
curl -i "http://127.0.0.1:8080/cgi-bin/hello.py?name=test"
```

POST-запрос должен вернуть `200 OK` и показать переданное тело `a=1&b=2`:

```bash
curl -i -X POST -H "Content-Type: application/x-www-form-urlencoded" --data "a=1&b=2" http://127.0.0.1:8080/cgi-bin/hello.py
```

`slow.py` должен вернуть `504 Gateway Timeout` примерно через пять секунд:

```bash
curl --max-time 10 -i http://127.0.0.1:8080/cgi-bin/slow.py
```

Проверьте загрузку и удаление файла. Ожидаемые статусы: `201`, `200`, `204`:

```bash
curl -i -X POST --data-binary "check" http://127.0.0.1:8080/uploads/check.txt
curl -i http://127.0.0.1:8080/uploads/check.txt
curl -i -X DELETE http://127.0.0.1:8080/uploads/check.txt
```

Чтобы остановить сервер, вернитесь в первое окно и нажмите `Ctrl+C`.
