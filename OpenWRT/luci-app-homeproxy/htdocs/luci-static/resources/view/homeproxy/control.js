'use strict';
'require view';
'require fs';
'require ui';

// Home Proxy: строка подключения трея к службе на этом роутере — hp-router или vps-client (какая
// установлена). Показать и скопировать, если забыли, или выпустить новую (новый ключ: прежние
// строки перестают работать сразу, трей на ПК попросит вставить новую). Сам ключ хранит служба
// (control.key рядом с настройками), страница его только показывает через `<служба> --config
// <настройки> --connection-string`. Адрес в строке — CONTROL_ADDR из настроек службы (адрес LAN
// роутера, например 192.168.1.1:47001): трей на ПК подключается напрямую, ssh-туннель не нужен.

var SERVICES = [
	{ name: 'hp-router', bin: '/usr/bin/hp-router', conf: '/etc/hp-router/router.env' },
	{ name: 'vps-client', bin: '/usr/bin/vps-client', conf: '/etc/vps-client/vps-client.conf' }
];

function exec(service, flag) {
	return fs.exec(service.bin, [ '--config', service.conf, flag ]).then(function(res) {
		if (res.code !== 0)
			throw new Error((res.stderr || res.stdout || '').trim() || (service.name + ': код ' + res.code));
		return (res.stdout || '').trim();
	});
}

// Службы пробуем по порядку; первая, что ответила, и есть служба этого роутера. Если не ответила
// ни одна — показываем ошибку той, что установлена (не «команда не найдена»).
function find(flag) {
	var errors = [];
	return SERVICES.reduce(function(chain, service) {
		return chain.then(function(found) {
			if (found)
				return found;
			return exec(service, flag).then(function(text) {
				return { service: service, text: text };
			}, function(e) {
				errors.push(e);
				return null;
			});
		});
	}, Promise.resolve(null)).then(function(found) {
		if (found)
			return found;
		var real = errors.filter(function(e) { return !/not found|ENOENT|No such file|NotFound|Entry not found/i.test(e.message); });
		throw (real[0] || new Error('На роутере нет службы Home Proxy (ни hp-router, ни vps-client).'));
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
		return find('--connection-string').catch(function(e) { return e; });
	},

	render: function(found) {
		var ok = !(found instanceof Error);
		var service = ok ? found.service : null;
		var result = ok ? found.text : found;
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
			'disabled': !ok,
			'click': function() {
				ui.showModal('Новая строка подключения', [
					E('p', 'Будет выпущен новый ключ. Трей, подключённый по прежней строке, сразу потеряет связь с роутером — в нём нужно будет вставить новую строку. Телефоны это не затрагивает.'),
					E('div', { 'class': 'right' }, [
						E('button', { 'class': 'cbi-button', 'click': ui.hideModal }, [ 'Отмена' ]),
						' ',
						E('button', {
							'class': 'cbi-button cbi-button-negative',
							'click': function() {
								return exec(service, '--new-connection-string').then(function(text) {
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
				'Строка подключения связывает трей Home Proxy на ПК с этим роутером' + (ok ? ' (служба ' + service.name + ')' : '') +
				': в ней адрес управления в LAN и ключ канала, ssh-туннель не нужен. ' +
				'Трей просит её при первом запуске (или кнопка «Служба…»). Строка — секрет: она открывает управление службой' +
				(ok && service.name === 'hp-router' ? ', в том числе добавление и удаление телефонов.' : ' (состояние, дыры, потери).')),
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
