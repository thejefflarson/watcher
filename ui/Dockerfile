# Build the static UI, serve it from an unprivileged nginx (listens on :8080).
# VITE_API_BASE="" makes the UI call the API same-origin (/api, /v1) so it can sit
# behind one ingress with the server.
FROM node:22-alpine AS build
WORKDIR /app
COPY package.json package-lock.json ./
RUN npm ci
COPY . .
ARG VITE_API_BASE=""
RUN VITE_API_BASE="$VITE_API_BASE" npm run build

FROM nginxinc/nginx-unprivileged:1.27-alpine
COPY nginx.conf /etc/nginx/conf.d/default.conf
COPY --from=build /app/dist /usr/share/nginx/html
EXPOSE 8080
