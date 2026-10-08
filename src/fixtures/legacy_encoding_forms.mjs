// HTML #picking-an-encoding-for-the-form, #submit-body, #submit-mutate-action
// and #encoding-parsing-a-url in a windows-1252 Document. URL
// #concept-urlencoded-serializer and Encoding #encode ("html" error mode)
// write unencodable code points as numeric character references.
(() => {
    const check = (condition, message) => { if (!condition) throw new Error(message); };
    const html = document.createElement('html'), body = document.createElement('body');
    document.append(html); html.append(body);
    check(document.characterSet === 'windows-1252', 'document encoding ' + document.characterSet);
    const submit = (form, submitter) => {
        form.requestSubmit(submitter);
        const [f, s] = __trust.takeFormSubmit().split(',');
        return JSON.parse(__trust.formSubmission(+f, s ? +s : null));
    };
    const form = document.createElement('form');
    form.setAttribute('action', '/search?old=é');
    form.innerHTML = '<input name="q" value="café あ"><input type=hidden name=_charset_>' +
        '<textarea name="té"></textarea><button name=go value="&amp;#12354;">Go</button>';
    form.querySelector('textarea').value = 'a\nb';
    body.append(form);

    // GET: the query is the form-urlencoded serialization in windows-1252.
    let r = submit(form);
    check(r.method === 'get', 'method ' + r.method);
    check(r.body === 'q=caf%E9+%26%2312354%3B&_charset_=windows-1252&t%E9=a%0D%0Ab', 'GET query ' + r.body);
    check(r.action === 'https://example.com/search?old=%E9', 'action ' + r.action);
    // A literal "&#12354;" is percent-encoded like any other text.
    r = submit(form, form.querySelector('button'));
    check(r.body.endsWith('&go=%26%2312354%3B'), 'submitter ' + r.body);

    // accept-charset: the first known label wins, after output-encoding.
    form.method = 'post';
    form.acceptCharset = 'bogus shift_jis utf-8';
    r = submit(form);
    check(r.enctype === 'application/x-www-form-urlencoded', 'enctype ' + r.enctype);
    check(r.body === 'q=caf%26%23233%3B+%82%A0&_charset_=Shift_JIS&t%26%23233%3B=a%0D%0Ab', 'POST urlencoded ' + r.body);
    form.acceptCharset = 'unknown-label';
    check(submit(form).body.includes('_charset_=UTF-8&t%C3%A9='), 'unknown accept-charset is UTF-8');
    form.acceptCharset = 'utf-16le';
    check(submit(form).body.includes('_charset_=UTF-8'), 'UTF-16 outputs as UTF-8');

    // text/plain: encoded bytes with numeric character references.
    form.acceptCharset = 'shift_jis';
    form.enctype = 'text/plain';
    r = submit(form);
    check(r.enctype === 'text/plain', 'text/plain enctype ' + r.enctype);
    check(r.body === 'q=caf&#233; \x82\xa0\r\n_charset_=Shift_JIS\r\nt&#233;=a\r\nb\r\n', 'text/plain ' + JSON.stringify(r.body));

    // multipart/form-data: names, values and filenames in the encoding.
    form.removeAttribute('accept-charset');
    form.enctype = 'multipart/form-data';
    r = submit(form);
    check(/^multipart\/form-data; boundary=/.test(r.enctype), 'multipart enctype ' + r.enctype);
    check(r.body.includes('Content-Disposition: form-data; name="q"\r\n\r\ncaf\xe9 &#12354;\r\n'), 'multipart value ' + JSON.stringify(r.body));
    check(r.body.includes('name="t\xe9"\r\n\r\na\r\nb\r\n'), 'multipart name ' + JSON.stringify(r.body));
    check(r.body.includes('name="_charset_"\r\n\r\nwindows-1252\r\n'), 'multipart charset');
    // A selected file's filename is encoded too (with LF, CR and '"'
    // escaped afterwards); its Content-Type and contents are bytes as they are.
    const upload = document.createElement('input');
    upload.type = 'file';
    upload.name = 'up';
    const transfer = new DataTransfer();
    transfer.items.add(new File(['x\u00e9'], 'caf\u00e9 \u3042"\n.txt', {type: 'text/plain'}));
    upload.files = transfer.files;
    form.append(upload);
    r = submit(form);
    check(r.body.includes('name="up"; filename="caf\xe9 &#12354;%22%0A.txt"\r\nContent-Type: text/plain\r\n\r\nx\xc3\xa9\r\n'),
        'multipart filename ' + JSON.stringify(r.body));
    upload.remove();

    // FormData itself stays UTF-8 (Fetch #concept-bodyinit-extract).
    const data = new FormData(form);
    check(data.get('q') === 'café あ', 'FormData value');

    // Encoding-parse a URL: element URLs, hyperlinks and History take the
    // document's encoding for the query; paths, fragments, ws: and the URL
    // API stay UTF-8.
    const a = document.createElement('a');
    a.href = '/pé?q=é#é';
    check(a.href === 'https://example.com/p%C3%A9?q=%E9#%C3%A9', 'a.href ' + a.href);
    check(a.search === '?q=%E9' && a.pathname === '/p%C3%A9', 'a.search ' + a.search);
    a.href = '?あ';
    check(a.href === 'https://example.com/?%26%2312354%3B', 'unencodable query ' + a.href);
    a.href = 'ws://h/?é';
    check(a.href === 'ws://h/?%C3%A9', 'ws query ' + a.href);
    const img = document.createElement('img');
    img.src = 'x.png?é';
    check(img.src === 'https://example.com/x.png?%E9', 'img.src ' + img.src);
    const link = document.createElement('link');
    link.href = 'x.css?é';
    check(link.href === 'https://example.com/x.css?%E9', 'link.href ' + link.href);
    check(new URL('?é', 'https://example.com/').href === 'https://example.com/?%C3%A9', 'URL API');
    check(new Request('/r?é').url === 'https://example.com/r?%C3%A9', 'Request URL');
    history.replaceState(null, '', '/h?é');
    check(location.href === 'https://example.com/h?%E9', 'history ' + location.href);
    return 'legacy-encoding-forms-ok';
})()
