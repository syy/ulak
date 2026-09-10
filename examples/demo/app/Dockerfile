FROM alpine:3.20

# build-context test: these two files are BAKED into the image.
# If it built on the remote daemon, the context was tarred and sent over the network.
COPY marker.txt /baked/marker.txt
COPY probe.sh   /usr/local/bin/probe
RUN chmod +x /usr/local/bin/probe

CMD ["probe"]
