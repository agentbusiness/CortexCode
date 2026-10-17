# fixture: a build stage whose WORKDIR does not carry over
FROM ubuntu:24.04 AS build
WORKDIR /src
RUN echo built > /src/artifact

FROM ubuntu:24.04
WORKDIR /app
WORKDIR project
COPY --from=build /src/artifact /app/project/artifact
