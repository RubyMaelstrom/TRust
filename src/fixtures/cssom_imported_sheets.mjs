// CSSOM #css-style-sheets, #the-stylesheet-interface,
// #the-cssimportrule-interface and CSS Cascade 4 #fetch-an-import: linked
// and imported sheets are CSSStyleSheets with their own location, parent,
// owner rule and origin-clean flag; CSSOM edits of an imported sheet reach
// the cascade.
(() => {
    const check = (condition, message) => { if (!condition) throw new Error(message); };
    const thrown = (f) => { try { f(); return 'none'; } catch (e) { return e.name; } };
    const sheets = document.styleSheets;
    check(sheets.length === 4, 'styleSheets ' + sheets.length);
    const [main, foreign, inline, unclean] = [sheets[0], sheets[1], sheets[2], sheets[3]];
    const link = document.querySelector('link');

    // StyleSheet is the abstract base of CSSStyleSheet.
    check(typeof StyleSheet === 'function' && main instanceof StyleSheet && main instanceof CSSStyleSheet, 'StyleSheet base');
    check(Object.getPrototypeOf(CSSStyleSheet.prototype) === StyleSheet.prototype, 'prototype chain');
    check(thrown(() => new StyleSheet()) === 'TypeError', 'StyleSheet has no constructor');
    check(!Object.prototype.hasOwnProperty.call(main, 'ownerNode'), 'ownerNode is an IDL attribute');
    check(thrown(() => StyleSheet.prototype.href) === 'TypeError', 'attribute receiver check');
    check(main.type === 'text/css', 'type');
    check(main.href === 'https://example.com/css/main.css', 'link href ' + main.href);
    check(main.ownerNode === link && link.sheet === main, 'ownerNode');
    check(main.parentStyleSheet === null && main.ownerRule === null, 'top-level parent');
    check(main.title === 'Main', 'title ' + main.title);
    check(inline.title === null && inline.href === null, 'style sheet title/href');

    // The import rules and their associated sheets.
    check(main.cssRules.length === 3, 'main rules ' + main.cssRules.length);
    const theme = main.cssRules[0];
    check(theme instanceof CSSImportRule && theme.type === 3, 'CSSImportRule');
    check(theme.href === 'theme.css', 'import href ' + theme.href);
    check(theme.media.mediaText === 'print', 'import media ' + theme.media.mediaText);
    check(theme.layerName === null && theme.supportsText === null, 'no layer/supports');
    check(theme.cssText === '@import url("theme.css") print;', 'import cssText ' + theme.cssText);
    const themeSheet = theme.styleSheet;
    check(themeSheet instanceof CSSStyleSheet && theme.styleSheet === themeSheet, 'styleSheet [SameObject]');
    check(themeSheet.href === 'https://example.com/css/theme.css', 'imported href ' + themeSheet.href);
    check(themeSheet.parentStyleSheet === main && themeSheet.ownerRule === theme, 'imported parent/owner rule');
    check(themeSheet.ownerNode === null && themeSheet.title === null, 'imported owner node/title');
    check(themeSheet.media === theme.media, 'shared MediaList');
    check(themeSheet.cssRules.length === 2, 'imported rules ' + themeSheet.cssRules.length);
    const deep = themeSheet.cssRules[0];
    check(deep.parentStyleSheet === themeSheet && deep.styleSheet.parentStyleSheet === themeSheet, 'nested import');
    check(deep.styleSheet.href === 'https://example.com/css/deep.css', 'nested href ' + deep.styleSheet.href);
    check(deep.styleSheet.cssRules[0].selectorText === '.d', 'nested rules');
    check(main.cssRules[1].styleSheet === null, 'missing import has no sheet');
    check(main.cssRules[2].selectorText === '.a', 'later rule');

    // Origin-clean: a no-CORS cross-origin sheet's rules are unreadable,
    // and so are an imported sheet's whose response was cross-origin.
    check(foreign.href === 'https://cdn.example/x.css', 'foreign href');
    check(thrown(() => foreign.cssRules) === 'SecurityError', 'cross-origin cssRules');
    check(thrown(() => foreign.insertRule('p{}')) === 'SecurityError', 'cross-origin insertRule');
    const crossImport = unclean.cssRules[0].styleSheet;
    check(crossImport && thrown(() => crossImport.cssRules) === 'SecurityError', 'cross-origin import');

    // layer, supports() and media conditions of an inline style's import.
    const inlineImport = inline.cssRules[0];
    check(inlineImport.layerName === 'base', 'layerName ' + inlineImport.layerName);
    check(inlineImport.supportsText === 'display: grid', 'supportsText ' + inlineImport.supportsText);
    check(inlineImport.media.mediaText === 'screen', 'inline media ' + inlineImport.media.mediaText);
    check(inlineImport.cssText === '@import url("inline.css") layer(base) supports(display: grid) screen;', 'cssText ' + inlineImport.cssText);
    const inlineSheet = inlineImport.styleSheet;
    check(inlineSheet.href === 'https://example.com/inline.css', 'inline import href');
    check(inline.cssRules[1].layerName === '' && inline.cssRules[1].styleSheet === null, 'anonymous layer, false supports');
    check(inline.cssRules[1].supportsText === 'display: no-such-display', 'false supports text');

    // CSSOM edits of an imported sheet write back and restyle.
    const target = document.getElementById('target');
    check(getComputedStyle(target).color === 'rgb(255, 0, 0)', 'imported color ' + getComputedStyle(target).color);
    inlineSheet.insertRule('#target { color: rgb(1, 2, 3) }', 1);
    check(getComputedStyle(target).color === 'rgb(1, 2, 3)', 'edited import ' + getComputedStyle(target).color);
    inlineSheet.cssRules[1].style.color = 'rgb(4, 5, 6)';
    check(getComputedStyle(target).color === 'rgb(4, 5, 6)', 'edited declaration ' + getComputedStyle(target).color);
    inlineSheet.disabled = true;
    check(getComputedStyle(target).color === 'rgb(0, 0, 0)', 'disabled import ' + getComputedStyle(target).color);
    inlineSheet.disabled = false;
    inlineImport.media.mediaText = 'print';
    check(getComputedStyle(target).color === 'rgb(0, 0, 0)', 'import media ' + getComputedStyle(target).color);
    inlineImport.media.mediaText = '';
    check(getComputedStyle(target).color === 'rgb(4, 5, 6)', 'restored import');
    inline.deleteRule(0);
    check(getComputedStyle(target).color === 'rgb(0, 0, 0)', 'deleted import ' + getComputedStyle(target).color);
    check(inlineImport.parentStyleSheet === null && inlineSheet.parentStyleSheet === inline, 'removed import keeps its sheet');

    // CSSConditionRule is the base of the media and supports rules.
    const constructed = new CSSStyleSheet();
    constructed.replaceSync('@media all { a { color: red } } @supports (display: block) { b { color: red } }');
    const [media, supports] = [constructed.cssRules[0], constructed.cssRules[1]];
    check(media instanceof CSSConditionRule && media instanceof CSSGroupingRule, 'CSSConditionRule');
    check(supports instanceof CSSConditionRule && supports.conditionText === '(display: block)', 'supports conditionText ' + supports.conditionText);
    check(media.matches === true && supports.matches === true, 'matches');
    check(thrown(() => { 'use strict'; media.conditionText = 'print'; }) === 'TypeError', 'conditionText is readonly');
    check(constructed.href === document.baseURI && constructed.ownerNode === null, 'constructed location ' + constructed.href);
    check(thrown(() => constructed.insertRule('@import url(x.css);')) === 'SyntaxError', 'constructed import');
    return 'cssom-imported-sheets-ok';
})()
