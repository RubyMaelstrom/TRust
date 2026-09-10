// Original CSSOM regression: expose supported properties' actual initial
// values, preserving live cascade and inheritance. No network/site script.
(function () {
    function check(value, expected, label) {
        if (value !== expected) throw Error(label + ': expected ' + expected + ', got ' + value);
    }
    const html = document.createElement('html'), body = document.createElement('body');
    document.append(html); html.append(body);
    const parent = document.createElement('div'), child = document.createElement('div');
    body.append(parent); parent.append(child);
    const style = getComputedStyle(child);
    function expect(visibility, pointer, label) {
        check(style.visibility, visibility, label + ' visibility');
        check(style.getPropertyValue('pointer-events'), pointer, label + ' pointer-events');
        check(style.transform, 'none', label + ' non-inherited transform');
    }
    expect('visible', 'auto', 'initial');
    parent.style.visibility = 'hidden'; parent.style.pointerEvents = 'none';
    parent.style.transform = 'translateX(12px)';
    expect('hidden', 'none', 'inherited');
    child.style.visibility = 'visible'; child.style.pointerEvents = 'auto';
    expect('visible', 'auto', 'explicit override');
    child.style.visibility = 'initial'; child.style.pointerEvents = 'initial';
    child.style.transform = 'initial';
    expect('visible', 'auto', 'CSS-wide initial');
    child.style.visibility = 'inherit'; child.style.pointerEvents = 'inherit';
    expect('hidden', 'none', 'CSS-wide inherit');
    child.style.visibility = 'unset'; child.style.pointerEvents = 'unset';
    child.style.transform = 'unset';
    expect('hidden', 'none', 'CSS-wide unset');
    parent.style.removeProperty('visibility'); parent.style.removeProperty('pointer-events');
    expect('visible', 'auto', 'live return to initial');
    const sheet = document.createElement('style');
    sheet.textContent = '.hidden-parent { visibility: hidden; pointer-events: none; }';
    html.append(sheet); parent.className = 'hidden-parent';
    expect('hidden', 'none', 'stylesheet inheritance');
    parent.className = '';
    expect('visible', 'auto', 'stylesheet invalidation');
    const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
    body.append(svg);
    check(getComputedStyle(svg).visibility, 'visible', 'SVG visibility');
    check(getComputedStyle(svg).pointerEvents, 'auto', 'SVG pointer-events');
    check(style.getPropertyValue('not-a-css-property'), '', 'unknown property');
    return 'computed-style-initial-values-ok';
})();
