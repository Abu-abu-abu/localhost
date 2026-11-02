# Следующие шаги для проекта localhost

## Статус после интеграции

CGI-модуль и keep-alive уже подключены к проекту. Файл `src/cgi.rs` собран и его 7 unit-тестов проходят; весь проект проходит `cargo test --offline`. Keep-alive проверен несколькими запросами через один TCP-сокет. Весь процесс CGI дополнительно проверен временным Node CGI-скриптом: GET/query, POST-тело и тайм-аут 504 сработали. На ноутбуке `python` указывает на WindowsApps-заглушку, зато `py -V` показывает Python 3.13. Конфиг Windows использует `py`; Linux-конфиг использует `python3`. Нужно проверить предоставленные Python-скрипты и Linux-ветку epoll в университете, затем выполнить `siege`, сравнить с NGINX и проверить утечки памяти.

`localhost.conf` предназначен для Windows (`cgi .py=py`), а `localhost.linux.conf` — для Linux (`cgi .py=python3`).

Модуль объявлен в `src/lib.rs` как `pub mod cgi`, потому что сетевой сервер тоже находится в библиотеке; добавлять `mod cgi;` в `main.rs` не нужно. Ниже оставлен исходный план Клода как справочник. Его вставки используют примерные имена и не должны копироваться поверх уже подключённого кода.

Порядок: 1) CGI, 2) keep-alive, 3) Linux/epoll, 4) siege, 5) утечки, 6) тесты.
Имена полей и функций ниже (`conn`, `route`, `error_response` и т.д.) примерные: замените на свои.

---

## 1. Подключить CGI (в проекте уже выполнено)

1. Положите `cgi.rs` в `src/cgi.rs`, в `main.rs` добавьте `mod cgi;`.
2. Положите `hello.py` и `slow.py` в каталог, который в конфиге указан как CGI-маршрут (например `www/cgi-bin/`).
3. В конфиге маршрута нужны расширение (`.py`) и команда интерпретатора: `py` на этом Windows-ноутбуке или `python3` на Linux.
4. В структуре соединения добавьте поле `cgi: Option<cgi::CgiJob>` и состояние «ждём CGI».

### Запуск CGI из обработчика маршрута

Когда маршрут совпал и расширение файла совпало с CGI-расширением:

```rust
use std::time::Duration;

let cgi_req = cgi::CgiRequest {
    interpreter: &route.cgi_interpreter,            // "python3" / "python"
    script_path: &script_path,                      // &Path: root маршрута + остаток URL
    method: &request.method,
    request_uri: &request.uri,                      // с query
    query_string: &request.query,                   // без '?'
    script_name: &request.path,
    path_info: &script_path.to_string_lossy(),      // полный путь, как просит задание
    headers: &request.headers,                      // &[(String, String)]
    body: &request.body,                            // уже собранное тело, без chunked-обёртки
    server_name: &server.name,
    server_port: server.port,
    remote_addr: &conn.peer_ip,
};

match cgi::CgiJob::spawn(&cgi_req, Duration::from_secs(5)) {
    Ok(job) => {
        conn.cgi = Some(job);
        conn.state = State::WaitingCgi;             // пока ничего клиенту не пишем
    }
    Err(code) => conn.set_response(error_response(code)),   // 404 или 500
}
```

Важно: для chunked-запросов сначала склейте тело (вы это уже делаете), и только потом передавайте его в `body`.

### Опрос CGI в главном цикле

Опрос сокетов делайте с коротким таймаутом, пока есть активные CGI, иначе `poll()` не будет вызываться вовремя:

```rust
let any_cgi = conns.values().any(|c| c.cgi.is_some());
let timeout_ms = if any_cgi { 10 } else { 1000 };
// ... ваш вызов epoll_wait / WSAPoll с timeout_ms ...
// ... обработка событий сокетов ...

// после обработки событий:
for conn in conns.values_mut() {
    let Some(job) = conn.cgi.as_mut() else { continue };
    match job.poll() {
        cgi::CgiState::Running => {}
        cgi::CgiState::Done(resp) => {
            conn.cgi = None;                         // Drop удалит временные файлы
            conn.set_response_from_cgi(resp.status, resp.headers, resp.body);
        }
        cgi::CgiState::Failed(code) => {
            conn.cgi = None;
            conn.set_response(error_response(code)); // 500 или 504
        }
    }
}
```

Что должен делать `set_response_from_cgi`: взять статус и заголовки из `resp`, добавить `Content-Length: <длина тела>`, добавить `Content-Type: text/plain`, если CGI его не вернул, и перевести соединение в состояние «пишем ответ».

Если клиент закрыл соединение, пока CGI работал, просто удалите соединение из таблицы: `Drop` у `CgiJob` убьёт процесс и очистит файлы.

Код 504 нужно добавить в список страниц ошибок (или отображать его как 500, если не хотите отдельную страницу).

### Проверка CGI

```
curl.exe -i "http://127.0.0.1:8080/cgi-bin/hello.py?name=test"
curl.exe -i -X POST -d "a=1&b=2" http://127.0.0.1:8080/cgi-bin/hello.py
curl.exe -i -H "Transfer-Encoding: chunked" -d "hello" http://127.0.0.1:8080/cgi-bin/hello.py
curl.exe -i http://127.0.0.1:8080/cgi-bin/slow.py        # через ~5 секунд должен быть 504
curl.exe -i http://127.0.0.1:8080/cgi-bin/net_takogo.py  # 404
```

В ответе `hello.py` проверьте: `cwd` равен каталогу скрипта, `argv` содержит имя файла, `body` совпадает с отправленным.

---

## 2. Keep-alive (в проекте уже реализовано; ниже — справочная схема)

Решение, оставлять ли соединение:

```rust
fn wants_keep_alive(version: &str, connection: Option<&str>) -> bool {
    let v = connection.map(|s| s.to_ascii_lowercase()).unwrap_or_default();
    if version == "HTTP/1.0" {
        v.contains("keep-alive")
    } else {
        !v.contains("close")
    }
}
```

В каждый ответ добавляйте `Content-Length` и `Connection: keep-alive` или `Connection: close` (по результату функции выше).

Когда ответ полностью отправлен:

```rust
if conn.keep_alive && !conn.close_after_write {
    conn.read_buf.drain(..conn.consumed);   // оставить байты следующего запроса, если они уже пришли
    conn.write_buf.clear();
    conn.write_pos = 0;
    conn.state = State::ReadingRequest;
    conn.last_activity = Instant::now();
    // переключить интерес в epoll/WSAPoll обратно на чтение
    // если в read_buf уже лежит целый следующий запрос, разберите его сразу, не ждите события
} else {
    close_connection(conn);                 // убрать из epoll, закрыть сокет, удалить из таблицы
}
```

`close_after_write = true` ставьте для ответов на битые запросы (400, 413, 500): состояние потока после них ненадёжно.

Таймауты (требование «все запросы имеют таймаут»), проверять в каждой итерации цикла:
- простаивающее keep-alive соединение: закрывать через 10–15 секунд без активности;
- медленный клиент, который не дочитал заголовки или тело: закрывать через 10–30 секунд, лучше отдав 408 или просто закрыв;
- CGI: уже есть (5 секунд в примере).

Проверка:

```
curl.exe -v http://127.0.0.1:8080/ http://127.0.0.1:8080/index.html
```

В выводе должно быть `Re-using existing connection`.

---

## 3. Linux / epoll (проверить обязательно, аудитор запускает на Linux)

На Windows это проще всего сделать через WSL:

```
wsl
cd /mnt/c/Users/<ваш_пользователь>/Desktop/LocalHost
cargo build
cargo test
cargo run -- <ваш_конфиг>
```

Что проверить:
- сервер стартует и отвечает на `curl`;
- в главном цикле ровно один вызов `epoll_wait` за итерацию, и все чтения/записи сокетов идут по событиям;
- сокеты и `epoll_fd` неблокирующие (`O_NONBLOCK`), закрытые соединения удаляются из epoll;
- `strace -f -e trace=epoll_wait,accept4,read,write ./target/debug/<бинарник> <конфиг>` показывает, что чтение и запись выполняются только после `epoll_wait`.

Чтение статических файлов с диска синхронное. Для обычных файлов epoll не работает, поэтому это нормально, главное ограничить размер читаемого файла и не читать в цикле без ограничений.

---

## 4. Нагрузочный тест (Linux / WSL)

```
sudo apt install siege
siege -b -t30S http://127.0.0.1:8080/
```

Смотрите строку `Availability`: нужно не меньше 99.5%. Повторите для маршрута с CGI, для 404 и для порта второго сервера.

Если появляются ошибки `Too many open files`, значит, соединения где-то не закрываются: проверьте удаление из таблицы после ошибок чтения/записи и по таймауту.

Сравнение с NGINX: запустите NGINX с похожим конфигом и сравните статусы и заголовки ответов (`curl -i`) для одних и тех же URL: 200, 404, 405, 301, листинг каталога.

---

## 5. Утечки памяти

```
valgrind --leak-check=full ./target/debug/<бинарник> <конфиг>
# в другом окне: siege -b -t20S http://127.0.0.1:8080/ ; затем Ctrl+C у сервера
```

Дополнительно во время siege следите за памятью процесса (`top` или `ps`): она не должна расти без конца. Обычно причины в Rust такие: соединения не удаляются из таблицы, буферы чтения/записи без ограничений, незавершённые CGI. Для первых двух поставьте лимиты (размер заголовков, размер тела из конфига).

---

## 6. Тесты (требование задания)

В `cgi.rs` уже есть 7 тестов. Проверьте, что в проекте есть тесты для всех четырёх пунктов задания:
- разбор запроса (GET/POST/DELETE, chunked из сырых байтов);
- проверка конфигурации (конфликт портов, неверные пути, неверный лимит тела);
- сопоставление маршрута (root, CGI, редирект);
- коды статусов для битых запросов и отсутствующих файлов.

Запуск: `cargo test --offline`.

---

## Итоговый чек-лист перед сдачей

- [ ] CGI работает для GET и POST, тело и query доходят до скрипта
- [ ] CGI не зависает навсегда (504 по таймауту)
- [ ] keep-alive и таймауты простаивающих соединений
- [ ] сервер не падает на битых запросах (проверьте `curl --data-binary @/dev/urandom` и обрыв соединения посреди запроса)
- [ ] страницы ошибок 400, 403, 404, 405, 413, 500
- [ ] несколько портов и несколько серверов, первый сервер для host:port по умолчанию
- [ ] siege: Availability >= 99.5%
- [ ] нет утечек (valgrind) и нет роста памяти
- [ ] `cargo test` проходит на Linux
