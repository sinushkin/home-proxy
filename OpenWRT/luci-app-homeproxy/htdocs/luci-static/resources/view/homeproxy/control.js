'use strict';
'require view';
'require fs';
'require ui';

// Home Proxy: строка подключения трея к hp-router. Показать и скопировать, если забыли, или
// выпустить новую (новый ключ: прежние строки перестают работать сразу, трей на ПК попросит
// вставить новую). Сам ключ хранит hp-router (/etc/hp-router/control.key), страница его только
// показывает через `hp-router --connection-string`.

var BIN = '/usr/bin/hp-router';
var CONF = '/etc/hp-router/router.env';

function run(flag) {
	return fs.exec(BIN, [ '--config', CONF, flag ]).then(function(res) {
		if (res.code !== 0)
			throw new Error((res.stderr || res.stdout || '').trim() || ('hp-router: код ' + res.code));
		return (res.stdout || '').trim();
	});
}

// navigator.clipboard есть только на https; LuCI часто открыт по http — тогда через выделение.
function copyText(text) {
	if (navigator.clipboard && window.isSecureContext)
		return navigator.clipboard.writeText(text);
	var area = E('textarea', { 'style': 'position:fixed;top:0;left:0;opacity:0' }, [ text ]);
	document.body.appendChild(area);
	area.select();
	try {
		if (!document.execCommand('copy'))
			throw new Error('копирование недоступно');
	} finally {
		document.body.removeChild(area);
	}
	return Promise.resolve();
}

return view.extend({
	load: function() {
		return run('--connection-string').catch(function(e) { return e; });
	},

	render: function(result) {
		var ok = typeof result === 'string';
		var field = E('input', {
			'type': 'password',
			'readonly': true,
			'class': 'cbi-input-text',
			'style': 'width:100%;font-family:monospace',
			'value': ok ? result : ''
		});
		var problem = E('div', { 'class': 'alert-message warning', 'style': ok ? 'display:none' : '' },
			[ ok ? '' : result.message ]);

		var show = E('button', {
			'class': 'cbi-button',
			'click': function() {
				var hidden = field.type === 'password';
				field.type = hidden ? 'text' : 'password';
				show.textContent = hidden ? 'Скрыть' : 'Показать';
			}
		}, [ 'Показать' ]);

		var copy = E('button', {
			'class': 'cbi-button cbi-button-action',
			'click': function() {
				return copyText(field.value).then(function() {
					ui.addNotification(null, E('p', 'Строка подключения скопирована — вставьте её в трей Home Proxy на ПК («Служба…»).'), 'info');
				}).catch(function(e) {
					field.type = 'text';
					field.select();
					ui.addNotification(null, E('p', 'Не удалось скопировать (' + e.message + '): строка выделена — скопируйте её вручную.'), 'warning');
				});
			}
		}, [ 'Скопировать' ]);

		var renew = E('button', {
			'class': 'cbi-button cbi-button-negative',
			'click': function() {
				ui.showModal('Новая строка подключения', [
					E('p', 'Будет выпущен новый ключ. Трей, подключённый по прежней строке, сразу потеряет связь с роутером — в нём нужно будет вставить новую строку. Телефоны это не затрагивает.'),
					E('div', { 'class': 'right' }, [
						E('button', { 'class': 'cbi-button', 'click': ui.hideModal }, [ 'Отмена' ]),
						' ',
						E('button', {
							'class': 'cbi-button cbi-button-negative',
							'click': function() {
								return run('--new-connection-string').then(function(text) {
									ui.hideModal();
									field.value = text;
									problem.style.display = 'none';
									ui.addNotification(null, E('p', 'Новая строка подключения готова. Прежние больше не действуют.'), 'info');
								}).catch(function(e) {
									ui.hideModal();
									ui.addNotification(null, E('p', 'Не удалось: ' + e.message), 'danger');
								});
							}
						}, [ 'Выпустить' ])
					])
				]);
			}
		}, [ 'Новая строка подключения' ]);

		return E('div', { 'class': 'cbi-map' }, [
			E('h2', 'Home Proxy'),
			E('div', { 'class': 'cbi-map-descr' },
				'Строка подключения связывает трей Home Proxy на ПК с этим роутером: в ней адрес управления и ключ канала. ' +
				'Трей просит её при первом запуске (или кнопка «Служба…»). Строка — секрет: с ней можно добавлять и удалять телефоны.'),
			problem,
			E('div', { 'class': 'cbi-section' }, [
				E('div', { 'class': 'cbi-value' }, [
					E('label', { 'class': 'cbi-value-title' }, 'Строка подключения'),
					E('div', { 'class': 'cbi-value-field' }, [ field ])
				]),
				E('div', { 'class': 'cbi-page-actions' }, [ show, ' ', copy, ' ', renew ])
			])
		]);
	},

	handleSaveApply: null,
	handleSave: null,
	handleReset: null
});
