FROM debian:13-slim
ARG PACKAGE_DIR=target/docker-package
COPY ${PACKAGE_DIR}/tier/bin/tier /opt/tier-package/bin/tier
COPY ${PACKAGE_DIR}/tier/lib/tier.so /opt/tier-package/lib/tier.so
COPY ${PACKAGE_DIR}/tier/web /opt/tier-package/web
COPY ${PACKAGE_DIR}/tier/deploy/install.sh ${PACKAGE_DIR}/tier/deploy/samba-version ${PACKAGE_DIR}/tier/deploy/tier.service ${PACKAGE_DIR}/tier/deploy/tier-hardware.service ${PACKAGE_DIR}/tier/deploy/tier-hardware.timer /opt/tier-package/deploy/
RUN apt-get update && apt-get install -y --no-install-recommends samba="$(cat /opt/tier-package/deploy/samba-version)" ca-certificates tini util-linux python3 && bash /opt/tier-package/deploy/install.sh && rm -rf /var/lib/apt/lists/* /opt/tier-package
COPY deploy/container.sh /usr/local/bin/tier-container
EXPOSE 445 9999
ENTRYPOINT ["/usr/bin/tini", "-g", "--", "bash", "/usr/local/bin/tier-container"]
