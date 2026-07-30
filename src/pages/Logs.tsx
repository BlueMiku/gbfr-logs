import { useMeterSettingsStore } from "@/stores/useMeterSettingsStore";
import "./Logs.css";

import { ActionIcon, AppShell, Burger, Group, NavLink, Text, Tooltip } from "@mantine/core";
import { useDisclosure } from "@mantine/hooks";
import { Gear, House, Megaphone } from "@phosphor-icons/react";
import { open } from "@tauri-apps/api/shell";
import { listen } from "@tauri-apps/api/event";
import { useEffect } from "react";
import { useTranslation } from "react-i18next";
import { Toaster } from "react-hot-toast";
import { Link, Outlet, useNavigate } from "react-router-dom";

const GITHUB_REPO_URL = "https://github.com/BlueMiku/gbfr-logs";

const Layout = () => {
  const { t } = useTranslation();
  const [mobileOpened, { toggle: toggleMobile }] = useDisclosure();
  const [desktopOpened, { toggle: toggleDesktop }] = useDisclosure(false);
  const { open_log_on_save } = useMeterSettingsStore((state) => ({ open_log_on_save: state.open_log_on_save }));

  const navigate = useNavigate();

  useEffect(() => {
    const debugListener = listen("debug-event", (event: { payload: unknown }) => {
      console.info(JSON.stringify(event.payload));
    });

    const saveListener = listen("encounter-saved", (event: { payload: number | null }) => {
      if (event.payload && open_log_on_save) {
        navigate(`/logs/${event.payload}`);
      }
    });

    return () => {
      debugListener.then((f) => f());
      saveListener.then((f) => f());
    };
  }, [open_log_on_save]);

  return (
    <div className="log-window">
      <AppShell
        header={{ height: 50 }}
        navbar={{
          width: 300,
          breakpoint: "sm",
          collapsed: { mobile: !mobileOpened, desktop: !desktopOpened },
        }}
        padding="sm"
      >
        <AppShell.Header>
          <Group h="100%" px="sm">
            <Burger opened={mobileOpened} onClick={toggleMobile} hiddenFrom="sm" size="sm" />
            <Burger opened={desktopOpened} onClick={toggleDesktop} visibleFrom="sm" size="sm" />
            <Text>GBFR Logs Satuuya Version</Text>
            <Tooltip label={t("ui.submit-missing-label")} color="dark">
              <ActionIcon
                aria-label="Submit missing label"
                variant="filled"
                color="light"
                ml="auto"
                onClick={() => open(`${GITHUB_REPO_URL}/issues/new?template=translation.yml`)}
              >
                <Megaphone size={16} />
              </ActionIcon>
            </Tooltip>
          </Group>
        </AppShell.Header>
        <AppShell.Navbar p="sm">
          <AppShell.Section grow>
            <NavLink label="Logs" leftSection={<House size="1rem" />} component={Link} to="/logs" />
          </AppShell.Section>
          <AppShell.Section>
            <NavLink label="Settings" leftSection={<Gear size="1rem" />} component={Link} to="/logs/settings" />
          </AppShell.Section>
        </AppShell.Navbar>
        <AppShell.Main>
          <Outlet />
        </AppShell.Main>
      </AppShell>
      <Toaster
        position="bottom-center"
        toastOptions={{
          style: {
            borderRadius: "10px",
            backgroundColor: "#252525",
            color: "#fff",
            fontSize: "14px",
          },
        }}
      />
    </div>
  );
};

export default Layout;
