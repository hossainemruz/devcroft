// Runs entirely inside the authored document. No host messages or capabilities.
(() => {
  let shell = document.querySelector('.shell');
  let main = shell && shell.querySelector(':scope > main');
  if (!main) {
    shell = document.createElement('div');
    shell.className = 'shell';
    main = document.createElement('main');
    for (const child of Array.from(document.body.childNodes)) {
      if (child.nodeType === 1 && child.matches('script, style')) continue;
      main.append(child);
    }
    shell.append(main);
    document.body.append(shell);
  }
  let toc = shell.querySelector(':scope > .toc');
  if (!toc) {
    toc = document.createElement('nav');
    toc.className = 'toc';
    shell.append(toc);
  }
  toc.setAttribute('aria-label', 'On this page');
  const authoredLabels = new Map();
  for (const link of toc.querySelectorAll('a[href^="#"]')) {
    let id;
    try { id = decodeURIComponent(link.hash.slice(1)); } catch { continue; }
    const target = document.getElementById(id);
    if (target) authoredLabels.set(target, link.textContent.trim());
  }
  const list = document.createElement('ol');
  const entries = [];
  for (const heading of main.querySelectorAll('h1, h2, h3')) {
    // Prefer a section's authored anchor so existing in-page links still work.
    const section = heading.closest('section[id]');
    const target = heading.id ? heading :
      section && section.querySelector('h1, h2, h3') === heading ? section : heading;
    if (!target.id) {
      let id = `devcroft-heading-${entries.length}`;
      while (document.getElementById(id)) id += '-';
      target.id = id;
    }
    const link = document.createElement('a');
    link.href = '#' + encodeURIComponent(target.id);
    link.textContent = authoredLabels.get(target) || heading.textContent.trim();
    link.title = link.textContent;
    link.style.paddingLeft = `${8 + (Number(heading.tagName.slice(1)) - 1) * 12}px`;
    link.addEventListener('click', event => {
      event.preventDefault();
      target.scrollIntoView({ block: 'start', behavior: 'instant' });
      // Keep keyboard focus in the outline for sequential navigation.
      update();
    });
    const item = document.createElement('li');
    item.append(link);
    list.append(item);
    entries.push({ target, link });
  }
  toc.replaceChildren(list);
  if (!entries.length) {
    const empty = document.createElement('p');
    empty.className = 'reader-empty';
    empty.textContent = 'No headings on this page';
    toc.append(empty);
  }
  let active;
  function update() {
    let current = entries[0];
    // Narrow browser copies stack the outline and article in the shell scroller.
    const scroller = getComputedStyle(main).overflowY === 'visible' ? shell : main;
    const top = scroller.getBoundingClientRect().top + 32;
    for (const entry of entries) {
      if (entry.target.getBoundingClientRect().top <= top) current = entry;
    }
    if (scroller.scrollTop > 0 && scroller.scrollTop + scroller.clientHeight >= scroller.scrollHeight - 2) {
      current = entries[entries.length - 1];
    }
    if (current === active) return;
    if (active) active.link.removeAttribute('aria-current');
    active = current;
    if (active) active.link.setAttribute('aria-current', 'location');
  }
  main.addEventListener('scroll', update, { passive: true });
  shell.addEventListener('scroll', update, { passive: true });
  window.addEventListener('resize', update);
  update();
})();
