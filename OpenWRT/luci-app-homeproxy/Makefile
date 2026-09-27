# Сборка ipk в фиде LuCI (OpenWrt SDK): скопировать каталог в feeds/luci/applications/.
# Без SDK — install.sh (файлы на роутер по ssh).
include $(TOPDIR)/rules.mk

LUCI_TITLE:=Home Proxy — строка подключения трея
LUCI_DEPENDS:=+luci-base
LUCI_PKGARCH:=all

include ../../luci.mk

# call BuildPackage - OpenWrt buildroot signature
