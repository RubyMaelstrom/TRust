// W3C Editing execCommand (docs/execCommand): Document.execCommand() and the
// queryCommand*() methods, the inline and block formatting commands,
// Input Events 2 beforeinput/input, undo/redo history and the clipboard
// commands' activation requirement (Clipboard API and events).
(() => {
    const failures = [];
    document.body.innerHTML = '<div id=ed contenteditable></div><p id=outside>outside</p><button id=button>b</button>';
    const ed = document.getElementById('ed');
    const sel = getSelection();
    // Text markers: [ and ] delimit the selection, | is a collapsed caret.
    function setup(html) {
        ed.innerHTML = html;
        let start = null, end = null;
        const walker = document.createTreeWalker(ed, NodeFilter.SHOW_TEXT);
        const texts = [];
        for (let node = walker.nextNode(); node; node = walker.nextNode()) texts.push(node);
        for (const text of texts) {
            for (let i = text.data.search(/[\[\]|]/); i >= 0; i = text.data.search(/[\[\]|]/)) {
                const mark = text.data[i];
                text.data = text.data.slice(0, i) + text.data.slice(i + 1);
                if (mark !== ']') start = [text, i];
                if (mark !== '[') end = [text, i];
            }
        }
        if (start) sel.setBaseAndExtent(start[0], start[1], end[0], end[1]);
    }
    // The editor's markup with the selection drawn as [ ] (in text), { }
    // (between nodes) or | (collapsed).
    function serialize(root = ed) {
        const range = sel.rangeCount ? sel.getRangeAt(0) : null;
        const mark = (node, offset) => {
            let text = '';
            if (range && range.startContainer === node && range.startOffset === offset)
                text += range.collapsed ? '|' : node.nodeType === 3 ? '[' : '{';
            if (range && !range.collapsed && range.endContainer === node && range.endOffset === offset)
                text += node.nodeType === 3 ? ']' : '}';
            return text;
        };
        const visit = (node) => {
            let text = '';
            if (node.nodeType === 3) {
                for (let i = 0; i <= node.data.length; i++) {
                    text += mark(node, i);
                    if (i < node.data.length) text += node.data[i] === '\xa0' ? '&nbsp;' : node.data[i];
                }
                return text;
            }
            const children = [...node.childNodes];
            for (let i = 0; i <= children.length; i++) {
                text += mark(node, i);
                const child = children[i];
                if (!child || child.nodeType === 8) continue;
                if (child.nodeType === 3) { text += visit(child); continue; }
                const attributes = [...child.attributes].map((attr) => ' ' + attr.name + '="' + attr.value + '"').join('');
                text += '<' + child.localName + attributes + '>' + visit(child) +
                    (['br', 'img', 'hr'].includes(child.localName) ? '' : '</' + child.localName + '>');
            }
            return text;
        };
        return visit(root);
    }
    function expect(label, actual, expected) {
        if (actual !== expected) failures.push(label + ': expected ' + JSON.stringify(expected) + ', got ' + JSON.stringify(actual));
    }
    function run(label, html, command, value, expected, extra) {
        setup(html);
        const result = command ? document.execCommand(command, false, value) : '';
        expect(label, result + ' ' + serialize() + (extra ? ' ' + extra() : ''), expected);
    }

    // #methods-to-query-and-execute-commands and #supported-commands.
    expect('lengths', [Document.prototype.execCommand.length, document.queryCommandValue.length].join(), '1,1');
    expect('supported', ['bold', 'BOLD', 'insertText', 'copy', 'cut', 'paste', 'heading', 'nonsense']
        .map((name) => document.queryCommandSupported(name)).join(), 'true,true,true,true,true,false,false,false');
    let thrown = 'none';
    try { document.execCommand(); } catch (error) { thrown = error.name; }
    expect('arguments', thrown, 'TypeError');
    sel.removeAllRanges();
    expect('no selection', [document.queryCommandEnabled('bold'), document.execCommand('bold'),
        document.queryCommandEnabled('selectAll'), document.queryCommandEnabled('undo')].join(), 'false,false,true,true');
    setup('<p>[foo]</p>');
    expect('enabled', [document.queryCommandEnabled('bold'), document.queryCommandEnabled('cut'),
        document.queryCommandEnabled('paste'), document.queryCommandValue('bold'),
        document.queryCommandState('fontName'), document.queryCommandIndeterm('createLink')].join(), 'true,true,false,,false,false');

    // Inline formatting commands (#inline-formatting-commands).
    run('bold', '<p>[foo]</p>', 'bold', '', 'true <p><b>[foo]</b></p> true', () => document.queryCommandState('bold'));
    run('unbold', '<p><b>[foo]</b></p>', 'bold', '', 'true <p>[foo]</p> false', () => document.queryCommandState('bold'));
    run('italic', '<p>[foo]</p>', 'italic', '', 'true <p><i>[foo]</i></p>');
    run('underline', '<p>[foo]</p>', 'underline', '', 'true <p><u>[foo]</u></p>');
    run('strikethrough', '<p>[foo]</p>', 'strikethrough', '', 'true <p><s>[foo]</s></p>');
    run('subscript', '<p>[foo]</p>', 'subscript', '', 'true <p><sub>[foo]</sub></p>');
    run('superscript replaces subscript', '<p><sub>[foo]</sub></p>', 'superscript', '', 'true <p><sup>[foo]</sup></p>');
    run('bold part', '<p>f[o]o</p>', 'bold', '', 'true <p>f<b>[o]</b>o</p>');
    run('push down', '<p><b>f[o]o</b></p>', 'bold', '', 'true <p><b>f</b>[o]<b>o</b></p>');
    run('indeterminate', '<p>[f<b>oo</b>]</p>', '', '', ' <p>[f<b>oo</b>]</p> true,false',
        () => [document.queryCommandIndeterm('bold'), document.queryCommandState('bold')].join());
    run('styleWithCSS', '<p>[foo]</p>', 'styleWithCSS', 'true',
        'true <p>[foo]</p> true <p><span style="font-weight: bold;">[foo]</span></p>',
        () => document.queryCommandState('styleWithCSS') + ' ' + (document.execCommand('bold'), document.execCommand('useCSS', false, 'true'), serialize()));
    run('foreColor', '<p>[foo]</p>', 'foreColor', 'red', 'true <p><font color="#ff0000">[foo]</font></p> rgb(255, 0, 0)',
        () => document.queryCommandValue('foreColor'));
    run('foreColor without #', '<p>[foo]</p>', 'foreColor', '00f', 'true <p><font color="#0000ff">[foo]</font></p>');
    run('foreColor invalid', '<p>[foo]</p>', 'foreColor', 'nonsense', 'false <p>[foo]</p>');
    run('foreColor currentColor', '<p>[foo]</p>', 'foreColor', 'currentColor', 'false <p>[foo]</p>');
    run('hiliteColor', '<p>[foo]</p>', 'hiliteColor', 'yellow',
        'true <p><span style="background-color: yellow;">[foo]</span></p> rgb(255, 255, 0)', () => document.queryCommandValue('backColor'));
    run('fontName', '<p>[foo]</p>', 'fontName', 'monospace', 'true <p><font face="monospace">[foo]</font></p> monospace',
        () => document.queryCommandValue('fontName'));
    run('fontSize', '<p>[foo]</p>', 'fontSize', '5', 'true <p><font size="5">[foo]</font></p> 5', () => document.queryCommandValue('fontSize'));
    run('fontSize relative', '<p>[foo]</p>', 'fontSize', '+2', 'true <p><font size="5">[foo]</font></p>');
    run('fontSize invalid', '<p>[foo]</p>', 'fontSize', 'big', 'false <p>[foo]</p>');
    run('fontSize value', '<p>f|oo</p>', '', '', ' <p>f|oo</p> 3', () => document.queryCommandValue('fontSize'));
    run('createLink', '<p>[foo]</p>', 'createLink', 'http://example.com/', 'true <p><a href="http://example.com/">[foo]</a></p>');
    run('createLink empty', '<p>[foo]</p>', 'createLink', '', 'false <p>[foo]</p>');
    run('unlink', '<p><a href="x">f[o]o</a></p>', 'unlink', '', 'true <p>f[o]o</p>');
    run('removeFormat', '<p><b>f[o]o</b></p>', 'removeFormat', '', 'true <p><b>f</b>[o]<b>o</b></p>');
    run('collapsed override', '<p>f|oo</p>', 'bold', '', 'true <p>f|oo</p> true <p>f<b>x|</b>oo</p>',
        () => document.queryCommandState('bold') + ' ' + (document.execCommand('insertText', false, 'x'), serialize()));
    run('override reset by selection', '<p>f|oo</p>', 'bold', '', 'true <p>f|oo</p> false',
        () => (sel.collapse(ed.firstChild.firstChild, 2), document.queryCommandState('bold')));

    // Block formatting commands (#block-formatting-commands).
    run('formatBlock', '<p>[foo]</p>', 'formatBlock', 'h1', 'true <h1>[foo]</h1> h1', () => document.queryCommandValue('formatBlock'));
    run('formatBlock brackets', '<div>[foo]</div>', 'formatBlock', '<P>', 'true <p>[foo]</p>');
    run('formatBlock invalid', '<p>[foo]</p>', 'formatBlock', 'span', 'false <p>[foo]</p>');
    run('insertParagraph', '<p>foo|bar</p>', 'insertParagraph', '', 'true <p>foo</p><p>|bar</p>');
    run('insertParagraph end', '<p>foo|</p>', 'insertParagraph', '', 'true <p>foo</p><p>|<br></p>');
    run('insertParagraph wraps', 'foo|bar', 'insertParagraph', '', 'true <div>foo</div><div>|bar</div>');
    run('insertParagraph heading end', '<h1>foo|</h1>', 'insertParagraph', '', 'true <h1>foo</h1><div>|<br></div>');
    run('insertParagraph pre', '<pre>foo|bar</pre>', 'insertParagraph', '', 'true <pre>foo<br>|bar</pre>');
    setup('<ul><li>a</li><li><br></li></ul>');
    sel.collapse(ed.firstChild.lastChild, 0);
    expect('insertParagraph empty item', document.execCommand('insertParagraph') + ' ' + serialize(),
        'true <ul><li>a</li></ul><div>|<br></div>');
    run('insertLineBreak', '<p>foo|bar</p>', 'insertLineBreak', '', 'true <p>foo<br>|bar</p>');
    run('insertLineBreak end', '<p>foo|</p>', 'insertLineBreak', '', 'true <p>foo<br>|<br></p>');
    run('insertText', '<p>foo|</p>', 'insertText', 'x', 'true <p>foox|</p>');
    run('insertText replaces', '<p>f[o]o</p>', 'insertText', 'xy', 'true <p>fxy|o</p>');
    run('insertText space', '<p>foo|</p>', 'insertText', ' ', 'true <p>foo&nbsp;|</p>');
    run('insertText spaces', '<p>foo|</p>', 'insertText', '  ', 'true <p>foo &nbsp;|</p>');
    run('insertText newline', '<p>foo|bar</p>', 'insertText', '\n', 'true <p>foo</p><p>|bar</p>');
    run('autolink', '<p>http://example.com|</p>', 'insertText', ' ',
        'true <p><a href="http://example.com">http://example.com</a>&nbsp;|</p>');
    run('autolink mail', '<p>me@example.com|</p>', 'insertText', ' ',
        'true <p><a href="mailto:me@example.com">me@example.com</a>&nbsp;|</p>');
    run('insertHTML', '<p>f[o]o</p>', 'insertHTML', '<b>bar</b>', 'true <p>f<b>bar</b>|o</p>');
    run('insertHTML block', '<p>f[o]o</p>', 'insertHTML', '<p>bar</p>', 'true <p>f</p><p>bar</p>|<p>o</p>');
    run('insertImage', '<p>f[o]o</p>', 'insertImage', 'a.png', 'true <p>f<img src="a.png">|o</p>');
    run('insertHorizontalRule', '<p>foo|bar</p>', 'insertHorizontalRule', '', 'true <p>foo</p><hr>|<p>bar</p>');
    run('insertOrderedList', '<p>[foo]</p>', 'insertOrderedList', '', 'true <ol><li>[foo]</li></ol> true,false <div>[foo]</div>',
        () => [document.queryCommandState('insertOrderedList'), document.queryCommandState('insertUnorderedList')].join() +
            ' ' + (document.execCommand('insertOrderedList'), serialize()));
    run('insertUnorderedList lines', '<p>[foo</p><p>bar]</p>', 'insertUnorderedList', '', 'true <ul><li>[foo</li><li>bar]</li></ul>');
    run('list conversion', '<ol><li>[foo]</li></ol>', 'insertUnorderedList', '', 'true <ul><li>[foo]</li></ul>');
    run('indent', '<p>[foo]</p>', 'indent', '', 'true <blockquote><p>[foo]</p></blockquote> <p>[foo]</p>',
        () => (document.execCommand('outdent'), serialize()));
    run('indent item', '<ol><li>a</li><li>[b]</li></ol>', 'indent', '',
        'true <ol><li>a</li><ol><li>[b]</li></ol></ol> <ol><li>a</li><li>[b]</li></ol>', () => (document.execCommand('outdent'), serialize()));
    run('justifyCenter', '<p>[foo]</p>', 'justifyCenter', '', 'true <div style="text-align: center;"><p>[foo]</p></div> true,center,false',
        () => [document.queryCommandState('justifyCenter'), document.queryCommandValue('justifyLeft'), document.queryCommandState('justifyLeft')].join());
    run('justify replaces', '<p>[foo]</p>', 'justifyRight', '', 'true <div style="text-align: right;"><p>[foo]</p></div> <p>[foo]</p>',
        () => (document.execCommand('justifyLeft'), serialize()));
    run('delete', '<p>foo|</p>', 'delete', '', 'true <p>fo|</p>');
    run('delete merges blocks', '<p>foo</p><p>|bar</p>', 'delete', '', 'true <p>foo|bar</p>');
    run('delete selection', '<p>f[oo</p><p>ba]r</p>', 'delete', '', 'true <p>f|r</p>');
    run('delete at host start', '<p>|foo</p>', 'delete', '', 'true <p>|foo</p>');
    run('delete line break', '<p>foo<br>|bar</p>', 'delete', '', 'true <p>foo|bar</p>');
    run('delete link', '<p><a href=x>foo</a>|bar</p>', 'delete', '', 'true <p>foo|bar</p>');
    run('delete list item', '<ul><li>|foo</li></ul>', 'delete', '', 'true <div>|foo</div>');
    run('forwardDelete', '<p>|foo</p>', 'forwardDelete', '', 'true <p>|oo</p>');
    run('forwardDelete merges', '<p>foo|</p><p>bar</p>', 'forwardDelete', '', 'true <p>foo|bar</p>');
    run('forwardDelete marks', '<p>|éx</p>', 'forwardDelete', '', 'true <p>|x</p>');

    // More of the specification's documented outcomes.
    run('formatBlock joins lines', '<p>[foo</p><p>bar]</p>', 'formatBlock', 'pre', 'true <pre>[foo<br>bar]</pre>');
    run('formatBlock paragraphs', '<h1>[foo</h1><h2>bar]</h2>', 'formatBlock', 'p', 'true <p>[foo</p><p>bar]</p>');
    run('delete joins lists', '<ul><li>f[oo</li></ul><ul><li>b]ar</li></ul>', 'delete', '', 'true <ul><li>f|ar</li></ul>');
    run('outdent keeps attributes', '<blockquote class=x><p>[foo]</p></blockquote>', 'outdent', '', 'true <div class="x"><p>[foo]</p></div>');
    run('list off', '<ol><li>[foo</li><li>bar]</li></ol>', 'insertOrderedList', '', 'true <div>[foo</div><div>bar]</div>');
    run('list item conversion', '<ol><li>foo</li><li>[bar]</li><li>baz</li></ol>', 'insertUnorderedList', '',
        'true <ol><li>foo</li></ol><ul><li>[bar]</li></ul><ol><li>baz</li></ol>');
    run('canonical spaces', '<p>foo |bar</p>', 'insertText', ' ', 'true <p>foo&nbsp; |bar</p>');
    run('delete non-breaking space', '<p>foo &nbsp;|bar</p>', 'delete', '', 'true <p>foo |bar</p>');
    run('bold across blocks', '<p>f[oo</p><p>ba]r</p>', 'bold', '', 'true <p>f<b>[oo</b></p><p><b>ba]</b>r</p>');
    run('italic removes em', '<p><em>[foo]</em></p>', 'italic', '', 'true <p>[foo]</p>');
    run('bold keeps attributes', '<p><strong class=x>[foo]</strong></p>', 'bold', '', 'true <p><span class="x">[foo]</span></p>');
    run('nested foreColor', '<p><font color=blue>f[o]o</font></p>', 'foreColor', 'red',
        'true <p><font color="blue">f<font color="#ff0000">[o]</font>o</font></p>');
    run('fontSize with CSS', '<p>[foo]</p>', 'styleWithCSS', 'true', 'true <p>[foo]</p> <p><span style="font-size: small;">[foo]</span></p>',
        () => (document.execCommand('fontSize', false, '2'), document.execCommand('styleWithCSS', false, 'false'), serialize()));
    run('push down underline', '<p><u>f[o]o</u></p>', 'underline', '', 'true <p><u>f</u>[o]<u>o</u></p>');
    run('push down line-through', '<p><span style="text-decoration: line-through">f[o]o</span></p>', 'strikethrough', '',
        'true <p><s>f</s>[o]<s>o</s></p>');
    run('createLink retargets', '<p><a href=old>f[o]o</a></p>', 'createLink', 'new', 'true <p><a href="new">f[o]o</a></p>');
    run('removeFormat block style', '<p style="font-weight: bold">[foo]</p>', 'removeFormat', '', 'true <p>[foo]</p>');
    run('alignment value', '<p style="text-align: right">[foo]</p>', '', '', ' <p style="text-align: right">[foo]</p> right,true',
        () => [document.queryCommandValue('justifyRight'), document.queryCommandState('justifyRight')].join());
    run('fontName indeterminate', '<p>[a<font face=serif>b</font>]</p>', '', '', ' <p>[a<font face="serif">b</font>]</p> true',
        () => document.queryCommandIndeterm('fontName'));
    run('list state mixed', '<ol><li>[a</li></ol><ul><li>b]</li></ul>', '', '', ' <ol><li>[a</li></ol><ul><li>b]</li></ul> true,false',
        () => [document.queryCommandIndeterm('insertOrderedList'), document.queryCommandState('insertOrderedList')].join());
    run('insertLineBreak in item', '<ul><li>foo|</li></ul>', 'insertLineBreak', '', 'true <ul><li>foo<br>|<br></li></ul>');
    run('insertParagraph in item', '<ul><li>foo|</li></ul>', 'insertParagraph', '', 'true <ul><li>foo</li><li>|<br></li></ul>');
    run('insertParagraph after term', '<dl><dt>foo|</dt></dl>', 'insertParagraph', '', 'true <dl><dt>foo</dt><dd>|<br></dd></dl>');
    run('forwardDelete selects table', '<p>foo|</p><table><tr><td>x</td></tr></table>', 'forwardDelete', '',
        'true <p>foo</p>{<table><tbody><tr><td>x</td></tr></tbody></table>}');
    run('delete image', '<p>foo<img src=a>|</p>', 'delete', '', 'true <p>foo|</p>');
    run('delete rule', '<p>foo</p><hr><p>|bar</p>', 'delete', '', 'true <p>foo</p><p>|bar</p>');
    run('superscript state', '<p><sup>[foo]</sup></p>', '', '', ' <p><sup>[foo]</sup></p> true,false,false',
        () => [document.queryCommandState('superscript'), document.queryCommandState('subscript'), document.queryCommandIndeterm('subscript')].join());

    // Miscellaneous commands (#miscellaneous-commands).
    run('selectAll', '<p>foo</p>', 'selectAll', '', 'true <p>foo</p> body,0,3',
        () => [sel.anchorNode.localName, sel.anchorOffset, sel.focusOffset].join());
    run('defaultParagraphSeparator', '<p>foo</p>', 'defaultParagraphSeparator', 'P', 'true <p>foo</p> p,false,true',
        () => [document.queryCommandValue('defaultParagraphSeparator'), document.execCommand('defaultParagraphSeparator', false, 'span'),
            document.execCommand('defaultParagraphSeparator', false, 'div')].join());

    // Input Events: beforeinput may cancel; input follows a DOM change.
    const events = [];
    const record = (event) => events.push(event.type + ':' + event.inputType + ':' + event.data + ':' +
        event.cancelable + ':' + event.getTargetRanges().length + ':' + event.isTrusted);
    ed.addEventListener('beforeinput', record);
    ed.addEventListener('input', record);
    setup('<p>[foo]</p>');
    document.execCommand('bold');
    expect('events', events.join(' '), 'beforeinput:formatBold:null:true:1:true input:formatBold:null:false:0:true');
    events.length = 0;
    const cancel = (event) => event.preventDefault();
    ed.addEventListener('beforeinput', cancel);
    setup('<p>[foo]</p>');
    expect('canceled', document.execCommand('italic') + ' ' + serialize() + ' ' + events.join(' '),
        'false <p>[foo]</p> beforeinput:formatItalic:null:true:1:true');
    ed.removeEventListener('beforeinput', cancel);
    events.length = 0;
    setup('<p>[foo]</p>');
    document.execCommand('styleWithCSS', false, 'false');
    expect('no events for miscellaneous commands', events.join(' '), '');

    // Undo and redo (#the-undo-command, Input Events historyUndo/historyRedo).
    setup('<p>[foo]</p>');
    document.execCommand('bold');
    document.execCommand('italic');
    events.length = 0;
    expect('undo', document.execCommand('undo') + ' ' + serialize(), 'true <p><b>[foo]</b></p>');
    expect('undo again', document.execCommand('undo') + ' ' + serialize(), 'true <p>[foo]</p>');
    expect('redo', document.execCommand('redo') + ' ' + serialize(), 'true <p><b>[foo]</b></p>');
    expect('history events', events.join(' '), 'beforeinput:historyUndo:null:true:0:true input:historyUndo:null:false:0:true ' +
        'beforeinput:historyUndo:null:true:0:true input:historyUndo:null:false:0:true ' +
        'beforeinput:historyRedo:null:true:0:true input:historyRedo:null:false:0:true');
    const bold = ed.querySelector('b');
    document.execCommand('undo');
    document.execCommand('redo');
    expect('redo keeps node identity', ed.querySelector('b') === bold, true);
    document.execCommand('insertText', false, 'x');
    expect('new edit clears redo', document.execCommand('redo') + ' ' + serialize(), 'false <p><b>x|</b></p>');
    ed.removeEventListener('beforeinput', record);
    ed.removeEventListener('input', record);

    // Native editing shares the history and applies overrides.
    setup('<p>foo|</p>');
    ed.focus();
    sel.collapse(ed.firstChild.firstChild, 3);
    __trust.editableKeyDefault(null, 'a', false, false, false, false);
    __trust.editableKeyDefault(null, 'b', false, false, false, false);
    expect('typing', serialize(), '<p>fooab|</p>');
    __trust.editableKeyDefault(null, 'z', false, true, false, false);
    expect('typing undone at once', serialize(), '<p>foo|</p>');
    __trust.editableKeyDefault(null, 'z', true, true, false, false);
    expect('typing redone', serialize(), '<p>fooab|</p>');
    document.execCommand('bold');
    __trust.editableKeyDefault(null, 'c', false, false, false, false);
    expect('typed override', serialize(), '<p>fooab<b>c|</b></p>');

    // plaintext-only editing hosts disable formatting commands.
    ed.setAttribute('contenteditable', 'plaintext-only');
    setup('<p>[foo]</p>');
    expect('plaintext-only', [document.queryCommandEnabled('bold'), document.execCommand('bold'),
        document.queryCommandEnabled('insertText')].join() + ' ' + serialize(), 'false,false,true <p>[foo]</p>');
    ed.setAttribute('contenteditable', '');

    // Clipboard commands need the script to be allowed to modify the
    // clipboard: transient activation.
    setup('<p>[foo]</p>');
    const clipboardEvents = [];
    ed.addEventListener('copy', (event) => clipboardEvents.push('copy:' + event.isTrusted));
    ed.addEventListener('cut', (event) => clipboardEvents.push('cut:' + event.isTrusted));
    expect('copy without activation', document.execCommand('copy') + ' ' + document.execCommand('cut') + ' ' + clipboardEvents.join(),
        'false false ');
    const button = document.getElementById('button');
    __trust.pointerButton(button.__id, true, 1, 1);
    __trust.pointerButton(button.__id, false, 1, 1);
    expect('copy', document.execCommand('copy') + ' ' + serialize(), 'true <p>[foo]</p>');
    expect('cut', document.execCommand('cut') + ' ' + serialize() + ' ' + clipboardEvents.join(), 'true <p>|<br></p> copy:true,cut:true');
    expect('paste', document.execCommand('paste'), false);

    // Commands do nothing outside editing hosts.
    sel.collapse(document.getElementById('outside').firstChild, 1);
    expect('outside', [document.queryCommandEnabled('bold'), document.execCommand('bold'),
        document.getElementById('outside').outerHTML].join(), 'false,false,<p id="outside">outside</p>');

    // designMode makes the document element an editing host.
    document.designMode = 'on';
    const outside = document.getElementById('outside');
    sel.setBaseAndExtent(outside.firstChild, 0, outside.firstChild, 3);
    expect('designMode', document.execCommand('bold') + ' ' + outside.innerHTML, 'true <b>out</b>side');
    document.designMode = 'off';

    if (failures.length) throw new Error(failures.join('\n'));
    return 'exec-command-ok';
})()
